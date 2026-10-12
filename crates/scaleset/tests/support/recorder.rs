//! A scaler that writes down what it is given, and can be told to wait or to fail.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_scaleset::{Error, Look, Message, Scaler, Statistics};
use tokio::sync::Notify;

/// What [`Scaler::renew`] answers.
pub type Renewal = Option<(Duration, Look)>;

#[derive(Debug, Clone)]
pub enum Seen {
    Initial(Statistics),
    Message(Message),
    Quiet,
}

#[derive(Clone, Default)]
pub struct Recorder {
    pub seen: Arc<Mutex<Vec<Seen>>>,
    /// Messages with these IDs make the scaler fail, once each.
    pub fail_on: Arc<Mutex<Vec<i64>>>,
    /// When set, a message waits here until the test notifies it.
    pub gate: Option<Arc<Notify>>,
    /// Signalled when a message has arrived (before the gate).
    pub arrived: Arc<Notify>,
    pub capacity: Arc<AtomicUsize>,
    /// What `renew` answers, one each time it is asked; `None` once they are used up.
    pub renewals: Arc<Mutex<VecDeque<Renewal>>>,
    /// How often `renew` was asked.
    pub asked: Arc<AtomicUsize>,
}

impl Recorder {
    pub fn new(capacity: usize) -> Recorder {
        Recorder {
            capacity: Arc::new(AtomicUsize::new(capacity)),
            ..Recorder::default()
        }
    }

    pub fn messages(&self) -> Vec<Message> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|s| match s {
                Seen::Message(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Scaler for Recorder {
    fn scale(&mut self, message: Option<&Message>) -> impl Future<Output = Result<usize, Error>> + Send {
        let seen = self.seen.clone();
        let fail_on = self.fail_on.clone();
        let gate = self.gate.clone();
        let arrived = self.arrived.clone();
        let capacity = self.capacity.load(Ordering::SeqCst);
        let message = message.cloned();
        async move {
            match message {
                None => seen.lock().unwrap().push(Seen::Quiet),
                Some(m) if m.id == 0 => seen
                    .lock()
                    .unwrap()
                    .push(Seen::Initial(m.statistics.unwrap_or_default())),
                Some(m) => {
                    arrived.notify_one();
                    if let Some(gate) = gate {
                        gate.notified().await;
                    }
                    let fail = {
                        let mut list = fail_on.lock().unwrap();
                        list.iter()
                            .position(|id| *id == m.id)
                            .map(|i| list.remove(i))
                            .is_some()
                    };
                    if fail {
                        return Err(Error::Scaler(format!("refused message {}", m.id)));
                    }
                    seen.lock().unwrap().push(Seen::Message(m));
                }
            }
            Ok(capacity)
        }
    }

    fn renew(&mut self) -> Renewal {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.renewals.lock().unwrap().pop_front().flatten()
    }
}
