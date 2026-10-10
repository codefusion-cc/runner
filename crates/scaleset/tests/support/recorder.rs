//! A scaler that writes down what it is given, and can be told to wait or to fail.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use cf_scaleset::{Error, Message, Scaler, Statistics};
use tokio::sync::Notify;

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
    pub capacity: usize,
}

impl Recorder {
    pub fn new(capacity: usize) -> Recorder {
        Recorder {
            capacity,
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
        let capacity = self.capacity;
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
}
