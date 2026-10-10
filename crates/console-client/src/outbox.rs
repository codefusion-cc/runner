use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::client::{RUNNER_NAME_MAX, units};
use crate::wire::JobEvent;

/// How many job events the Runner keeps for a console that cannot be reached. Over it the oldest is dropped.
pub const OUTBOX_CAPACITY: usize = 500;
/// How many job events one report carries from the outbox.
pub const REPORT_BATCH: usize = 100;
/// How many ids of events the console has taken are remembered, so that an event pushed again is not sent again.
pub const SENT_MEMORY: usize = 1000;
/// The longest event id.
pub const EVENT_ID_MAX: usize = 128;

/// What the file holds.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    waiting: Vec<JobEvent>,
    #[serde(default)]
    sent: Vec<String>,
    #[serde(default)]
    dropped: u64,
}

#[derive(Default)]
struct State {
    waiting: VecDeque<JobEvent>,
    waiting_ids: HashSet<String>,
    sent: VecDeque<String>,
    sent_ids: HashSet<String>,
    dropped: u64,
}

impl State {
    fn from_saved(saved: Saved, capacity: usize) -> State {
        let mut state = State {
            dropped: saved.dropped,
            ..State::default()
        };
        for id in saved.sent {
            state.remember(id);
        }
        for event in saved.waiting {
            if event.id.is_empty()
                || state.waiting_ids.contains(&event.id)
                || state.sent_ids.contains(&event.id)
            {
                continue;
            }
            state.waiting_ids.insert(event.id.clone());
            state.waiting.push_back(event);
        }
        while state.waiting.len() > capacity {
            state.drop_oldest();
        }
        state
    }

    fn saved(&self) -> Saved {
        Saved {
            waiting: self.waiting.iter().cloned().collect(),
            sent: self.sent.iter().cloned().collect(),
            dropped: self.dropped,
        }
    }

    fn drop_oldest(&mut self) {
        if let Some(event) = self.waiting.pop_front() {
            self.waiting_ids.remove(&event.id);
            self.dropped += 1;
        }
    }

    /// Notes that the console has this id, forgetting the oldest beyond [`SENT_MEMORY`].
    fn remember(&mut self, id: String) {
        if !self.sent_ids.insert(id.clone()) {
            return;
        }
        self.sent.push_back(id);
        while self.sent.len() > SENT_MEMORY {
            if let Some(old) = self.sent.pop_front() {
                self.sent_ids.remove(&old);
            }
        }
    }
}

/// Job events that wait for the console, kept in one file so that a Runner that restarts, or a console that is away
/// for an hour, loses none (up to the capacity).
///
/// Every event has an id that names it. An id that waits already, or that the console took among the last
/// [`SENT_MEMORY`], is not queued again. Over the capacity the oldest event is dropped and counted. The file is
/// replaced whole, for this user alone, on every change. All methods take `&self` and are safe to call from several
/// threads at once.
///
/// A write that fails is logged and the outbox goes on in memory: the events wait, and the next change writes again.
#[derive(Debug)]
pub struct Outbox {
    path: PathBuf,
    capacity: usize,
    state: Mutex<State>,
    delivery: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("waiting", &self.waiting.len())
            .field("sent", &self.sent.len())
            .field("dropped", &self.dropped)
            .finish()
    }
}

impl Outbox {
    /// The outbox in `path` (its folder is made, for this user alone, when it is not there), keeping at most
    /// `capacity` waiting events (at least 1).
    ///
    /// A file that cannot be read or is not an outbox is logged and set aside beside the file (`.corrupt`), and the
    /// outbox starts empty. One that holds more than `capacity` loses its oldest events, counted.
    pub fn open(path: impl Into<PathBuf>, capacity: usize) -> Outbox {
        let path = path.into();
        let capacity = capacity.max(1);
        if let Some(folder) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            use std::os::unix::fs::DirBuilderExt as _;
            if let Err(error) = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(folder)
            {
                tracing::error!(%error, "could not make the outbox's folder");
            }
        }
        let saved = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
                Ok(saved) => saved,
                Err(error) => {
                    tracing::error!(%error, "the outbox's file is not an outbox; set aside");
                    set_aside(&path);
                    Saved::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(error) => {
                tracing::error!(%error, "the outbox's file cannot be read; set aside");
                set_aside(&path);
                Saved::default()
            }
        };
        Outbox {
            path,
            capacity,
            state: Mutex::new(State::from_saved(saved, capacity)),
            delivery: tokio::sync::Mutex::new(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A thread that panicked holding the lock leaves a state that is whole (every change is one step).
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes the state; the caller holds the lock, so the file is always the newest state.
    fn persist(&self, state: &State) {
        let written = serde_json::to_vec(&state.saved())
            .map_err(std::io::Error::other)
            .and_then(|json| cf_secret::replace_private(&self.path, &json));
        if let Err(error) = written {
            tracing::error!(%error, "could not write the outbox");
        }
    }

    /// Queues an event. `false`, with nothing queued, when an event of that id waits already or the console took it
    /// among the last [`SENT_MEMORY`], and when the event could never be accepted: an empty or over-long id
    /// ([`EVENT_ID_MAX`]), or no Actions runner or one over 100 characters (these are logged). Texts the console cuts
    /// are cut here, so a report stays small. At the capacity the oldest waiting event is dropped and counted.
    pub fn push(&self, event: JobEvent) -> bool {
        let Some(event) = fit(event) else {
            return false;
        };
        let mut state = self.lock();
        if state.waiting_ids.contains(&event.id) || state.sent_ids.contains(&event.id) {
            return false;
        }
        while state.waiting.len() >= self.capacity {
            state.drop_oldest();
        }
        state.waiting_ids.insert(event.id.clone());
        state.waiting.push_back(event);
        self.persist(&state);
        true
    }

    /// The oldest waiting events, at most `max`. Nothing leaves the outbox until [`Outbox::sent`].
    pub fn batch(&self, max: usize) -> Vec<JobEvent> {
        self.lock().waiting.iter().take(max).cloned().collect()
    }

    /// Marks events the console has taken: they leave the outbox and their ids are remembered.
    pub fn sent<I, S>(&self, ids: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut state = self.lock();
        let ids: Vec<String> = ids.into_iter().map(|id| id.as_ref().to_string()).collect();
        if ids.is_empty() {
            return;
        }
        let taken: HashSet<&str> = ids.iter().map(String::as_str).collect();
        state.waiting.retain(|e| !taken.contains(e.id.as_str()));
        for id in &ids {
            state.waiting_ids.remove(id);
        }
        for id in ids {
            state.remember(id);
        }
        self.persist(&state);
    }

    /// How many events wait.
    pub fn len(&self) -> usize {
        self.lock().waiting.len()
    }

    /// Whether no event waits.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many events were dropped for want of room, over the outbox's life (it survives a restart).
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Waits for the turn to deliver: reports with events from this outbox go one after the other.
    pub(crate) async fn delivering(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.delivery.lock().await
    }
}

/// Keeps the file for a look and makes room for a new one.
fn set_aside(path: &Path) {
    let aside = path.with_extension("corrupt");
    if let Err(error) = std::fs::rename(path, &aside) {
        tracing::error!(%error, "could not set the outbox's file aside; it is replaced");
    }
}

fn cut(text: Option<String>, max: usize) -> Option<String> {
    text.map(|t| {
        if t.chars().count() > max {
            t.chars().take(max).collect()
        } else {
            t
        }
    })
}

/// The event as the console will read it, or `None` for one it would refuse (and so refuse every report it is in).
fn fit(mut event: JobEvent) -> Option<JobEvent> {
    if event.id.is_empty() || units(&event.id) > EVENT_ID_MAX {
        tracing::error!("a job event has no id, or one over {EVENT_ID_MAX} characters; not queued");
        return None;
    }
    if event.runner.is_empty() || units(&event.runner) > RUNNER_NAME_MAX {
        tracing::error!(
            "a job event names no Actions runner, or one over {RUNNER_NAME_MAX} characters; not queued"
        );
        return None;
    }
    event.repo = cut(event.repo, 200);
    event.job = cut(event.job, 300);
    event.workflow = cut(event.workflow, 400);
    event.event = cut(event.event, 50);
    event.result = cut(event.result, 30);
    event.end = cut(event.end, 50);
    if let Some(machine) = &mut event.machine {
        machine.size = machine.size.chars().take(64).collect();
        machine.origin = machine.origin.chars().take(64).collect();
    }
    Some(event)
}
