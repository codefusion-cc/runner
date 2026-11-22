//! The outside looks at game servers the console asks a host for ([`Answer::probes`]): each target an `A2S_INFO`
//! query (`cf-a2s`), every `probeEverySeconds`, on a socket of its own, all of a round at once, none tried again.
//! What they found goes with the next report ([`Report::probe_results`]) and leaves only once a report carrying it
//! was answered. An answer with no probes ends them: no round starts, and what waits is dropped.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cf_console_client::{
    Answer, MAX_PROBE_RESULTS, MAX_PROBE_TARGETS, PROBE_EVERY_BOUNDS_S, PROBE_EVERY_DEFAULT_S,
    PROBE_NAME_MAX, ProbeResult, ProbeTarget,
};

/// The one kind of target there is.
pub const KIND_A2S: &str = "a2s";
/// The most characters of why a look found no answer.
pub const ERROR_MAX: usize = 200;

/// The targets and what their looks found, between reports.
#[derive(Default)]
pub struct Probes {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    targets: Vec<ProbeTarget>,
    every: Duration,
    last: Option<Instant>,
    running: bool,
    pending: Vec<ProbeResult>,
}

impl Probes {
    /// Takes the targets of the console's answer, and returns them when a round is due now: none runs, and the last
    /// began at least `probeEverySeconds` ago (or never). The caller runs the round and hands its results to
    /// [`Probes::finished`].
    pub fn answered(&self, answer: &Answer, now: Instant) -> Option<Vec<ProbeTarget>> {
        let mut state = self.state.lock().expect("probes");
        state.targets = targets_of(answer);
        state.every = Duration::from_secs(
            answer
                .probe_every_seconds
                .unwrap_or(PROBE_EVERY_DEFAULT_S)
                .clamp(PROBE_EVERY_BOUNDS_S.0, PROBE_EVERY_BOUNDS_S.1),
        );
        if state.targets.is_empty() {
            // Turned off: nothing waits to be said of targets the console no longer asks about.
            state.pending.clear();
            return None;
        }
        let due = state
            .last
            .is_none_or(|last| now.duration_since(last) >= state.every);
        if state.running || !due {
            return None;
        }
        state.running = true;
        state.last = Some(now);
        Some(state.targets.clone())
    }

    /// A round's results, kept for the next report: of each target the newest, at most [`MAX_PROBE_RESULTS`] in all.
    /// Results of a target the console stopped asking about meanwhile are dropped.
    pub fn finished(&self, results: Vec<ProbeResult>) {
        let mut state = self.state.lock().expect("probes");
        state.running = false;
        let asked: HashSet<String> = state.targets.iter().map(|t| t.name.clone()).collect();
        for result in results.into_iter().filter(|r| asked.contains(&r.target)) {
            state.pending.retain(|kept| kept.target != result.target);
            state.pending.push(result);
        }
        let over = state.pending.len().saturating_sub(MAX_PROBE_RESULTS);
        state.pending.drain(..over);
    }

    /// What waits for the console, for a report.
    pub fn pending(&self) -> Vec<ProbeResult> {
        self.state.lock().expect("probes").pending.clone()
    }

    /// The console answered a report that carried `sent`: they wait no longer. One newer of the same target stays.
    pub fn sent(&self, sent: &[ProbeResult]) {
        self.state
            .lock()
            .expect("probes")
            .pending
            .retain(|kept| !sent.contains(kept));
    }
}

/// The targets of an answer the Runner looks at: kind `a2s`, a name of 1 to [`PROBE_NAME_MAX`] visible characters
/// once, a host [`cf_a2s::is_host`] takes, a port that is not 0; at most [`MAX_PROBE_TARGETS`] of them.
pub fn targets_of(answer: &Answer) -> Vec<ProbeTarget> {
    let mut names = HashSet::new();
    answer
        .probes
        .iter()
        .filter(|t| {
            t.kind == KIND_A2S
                && !t.name.is_empty()
                && t.name.chars().count() <= PROBE_NAME_MAX
                && !t.name.chars().any(|c| c.is_control() || c.is_whitespace())
                && cf_a2s::is_host(&t.host)
                && t.port != 0
                && names.insert(t.name.clone())
        })
        .take(MAX_PROBE_TARGETS)
        .cloned()
        .collect()
}

/// One round: every target at once, each within `timeout`. `at` is the round's start, RFC 3339.
pub async fn look(targets: Vec<ProbeTarget>, timeout: Duration, at: String) -> Vec<ProbeResult> {
    let mut round = tokio::task::JoinSet::new();
    for (i, target) in targets.iter().enumerate() {
        let (host, port) = (target.host.clone(), target.port);
        round.spawn(async move { (i, cf_a2s::query(&host, port, timeout).await) });
    }
    let mut results: Vec<Option<ProbeResult>> = vec![None; targets.len()];
    while let Some(done) = round.join_next().await {
        let Ok((i, answer)) = done else { continue };
        let target = targets[i].name.clone();
        results[i] = Some(match answer {
            Ok(answer) => ProbeResult {
                target,
                ok: true,
                at: at.clone(),
                latency_ms: Some(u64::try_from(answer.latency.as_millis()).unwrap_or(u64::MAX)),
                world_name: Some(answer.info.name),
                players: Some(u32::from(answer.info.players)),
                max: Some(u32::from(answer.info.max)),
                error: None,
            },
            Err(error) => ProbeResult {
                target,
                ok: false,
                at: at.clone(),
                latency_ms: None,
                world_name: None,
                players: None,
                max: None,
                error: Some(cf_guest_proto::clip(&error.to_string(), ERROR_MAX)),
            },
        });
    }
    results.into_iter().flatten().collect()
}
