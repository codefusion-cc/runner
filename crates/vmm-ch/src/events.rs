//! What cloud-hypervisor says about its machine through `--event-monitor`: one JSON object per event,
//! `{"timestamp": …, "source": "vm", "event": "booted", "properties": …}`, written one after the other.
//!
//! Only three events matter. `booted` is when [`crate::ChVmm`]'s start returns. `rebooting` is a guest that reset
//! its machine (a kernel panic restarts it at once, see [`cf_vmm::COMMAND_LINE`]): a machine runs one boot, so the
//! host ends it. `shutdown` comes before the monitor exits for a guest's power-off, when its writer gets to it: the
//! monitor's exit status is what says how the machine ended, never an event that may be lost.
//!
//! The stream is read as it comes, in pieces of any size. What cannot be read (a broken object, one larger than
//! [`MAX_EVENT`]) is skipped up to the next object; it never stops the reading of what follows.

use serde_json::Value;

/// The largest event kept while it is still coming: far above any event cloud-hypervisor writes.
pub const MAX_EVENT: usize = 64 << 10;

/// An event that matters to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The machine runs.
    Booted,
    /// The guest reset its machine.
    Rebooting,
    /// The guest powered its machine off.
    Shutdown,
    /// Anything else the monitor says.
    Other,
}

/// What came of the stream so far, read event by event.
#[derive(Debug, Default)]
pub struct Events {
    pending: Vec<u8>,
}

impl Events {
    /// Takes the next bytes of the stream and gives the events they complete, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        let mut found = Vec::new();
        loop {
            let mut stream = serde_json::Deserializer::from_slice(&self.pending).into_iter::<Value>();
            match stream.next() {
                Some(Ok(value)) => {
                    let used = stream.byte_offset();
                    found.push(kind(&value));
                    self.pending.drain(..used);
                }
                // The rest of an object is still to come.
                Some(Err(error)) if error.is_eof() => break,
                Some(Err(_)) => {
                    // Skip to the next object that starts a line, the way the monitor writes each one.
                    match self.pending.windows(2).skip(1).position(|pair| pair == b"\n{") {
                        Some(at) => drop(self.pending.drain(..at + 2)),
                        None => self.pending.clear(),
                    }
                }
                None => {
                    self.pending.clear();
                    break;
                }
            }
        }
        if self.pending.len() > MAX_EVENT {
            self.pending.clear();
        }
        found
    }
}

fn kind(value: &Value) -> Event {
    if value.get("source").and_then(Value::as_str) != Some("vm") {
        return Event::Other;
    }
    match value.get("event").and_then(Value::as_str) {
        Some("booted") => Event::Booted,
        Some("rebooting") => Event::Rebooting,
        Some("shutdown") => Event::Shutdown,
        _ => Event::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A start and a power-off as cloud-hypervisor writes them.
    const STREAM: &str = r#"{
  "timestamp": {
    "secs": 0,
    "nanos": 21875
  },
  "source": "vmm",
  "event": "starting",
  "properties": null
}
{
  "timestamp": {
    "secs": 0,
    "nanos": 9812041
  },
  "source": "vm",
  "event": "booting",
  "properties": null
}
{
  "timestamp": {
    "secs": 0,
    "nanos": 20718250
  },
  "source": "vm",
  "event": "booted",
  "properties": null
}
{
  "timestamp": {
    "secs": 3,
    "nanos": 1
  },
  "source": "vm",
  "event": "shutdown",
  "properties": null
}
"#;

    const ALL: [Event; 4] = [Event::Other, Event::Other, Event::Booted, Event::Shutdown];

    #[test]
    fn the_events_of_a_run_are_told_apart() {
        assert_eq!(Events::default().push(STREAM.as_bytes()), ALL);
    }

    #[test]
    fn a_reset_is_heard() {
        let reset =
            r#"{"timestamp":{"secs":1,"nanos":0},"source":"vm","event":"rebooting","properties":null}"#;
        assert_eq!(Events::default().push(reset.as_bytes()), [Event::Rebooting]);
    }

    #[test]
    fn an_event_of_another_source_with_a_known_name_is_not_taken() {
        let vmm = r#"{"source":"vmm","event":"shutdown"}{"source":"virtio-device","event":"booted"}"#;
        assert_eq!(
            Events::default().push(vmm.as_bytes()),
            [Event::Other, Event::Other]
        );
    }

    #[test]
    fn an_event_cut_in_pieces_is_told_once_whole() {
        let mut events = Events::default();
        let bytes = STREAM.as_bytes();
        let middle = STREAM.find("\"booted\"").unwrap();
        assert_eq!(events.push(&bytes[..middle]), [Event::Other, Event::Other]);
        assert_eq!(events.push(&bytes[middle..]), [Event::Booted, Event::Shutdown]);
    }

    #[test]
    fn garbage_is_skipped_and_what_follows_is_still_read() {
        let mut events = Events::default();
        let broken = format!("{{\"source\": \"vm\", \"event\": bo\n{STREAM}");
        assert_eq!(events.push(broken.as_bytes()), ALL);
        let mut events = Events::default();
        assert_eq!(events.push(b"\x00\xff not json at all"), []);
        assert_eq!(events.push(format!("\n{STREAM}").as_bytes()), ALL);
    }

    #[test]
    fn an_endless_event_is_dropped_and_costs_no_more_than_its_bound() {
        let mut events = Events::default();
        let mut endless = b"{\"source\": \"vm\", \"pad\": \"".to_vec();
        endless.extend(std::iter::repeat_n(b'x', MAX_EVENT));
        assert_eq!(events.push(&endless), []);
        assert!(events.pending.is_empty());
        assert_eq!(events.push(STREAM.as_bytes()), ALL);
    }

    #[test]
    fn values_that_are_not_events_are_other() {
        assert_eq!(Events::default().push(b"1 \"x\" [] null {}"), [Event::Other; 5]);
        assert_eq!(
            Events::default().push(b"{\"source\":\"vm\",\"event\":7}"),
            [Event::Other]
        );
    }

    proptest! {
        #[test]
        fn pieces_of_any_size_give_the_same_events(cuts in proptest::collection::vec(0usize..STREAM.len(), 0..12)) {
            let mut cuts = cuts;
            cuts.sort_unstable();
            let bytes = STREAM.as_bytes();
            let mut events = Events::default();
            let mut found = Vec::new();
            let mut from = 0;
            for cut in cuts.into_iter().chain([bytes.len()]) {
                found.extend(events.push(&bytes[from..cut.max(from)]));
                from = cut.max(from);
            }
            prop_assert_eq!(found, ALL.to_vec());
        }

        #[test]
        fn hostile_bytes_never_panic_and_stay_bounded(noise in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let mut events = Events::default();
            let _ = events.push(&noise);
            prop_assert!(events.pending.len() <= MAX_EVENT);
        }
    }
}
