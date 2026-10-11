//! The service's log: one line per event, in a file that is rolled over before it grows without end.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// The size past which the log is moved aside (to `<name>.1`, replacing the one before) and begun again.
pub const ROLL_AT: u64 = 10 << 20;

struct Sink {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    roll_at: u64,
}

impl Sink {
    fn open(&mut self) {
        if let Some(folder) = self.path.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.written = self
            .file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map_or(0, |meta| meta.len());
    }

    fn line(&mut self, line: &str) {
        if self.written >= self.roll_at {
            self.file = None;
            let mut aside = self.path.clone().into_os_string();
            aside.push(".1");
            let _ = std::fs::rename(&self.path, aside);
        }
        if self.file.is_none() {
            self.open();
        }
        // A log that cannot be written must not stop the service: the line goes to launchd's own file.
        let wrote = self
            .file
            .as_mut()
            .is_some_and(|file| file.write_all(line.as_bytes()).is_ok());
        if wrote {
            self.written += line.len() as u64;
        } else {
            eprint!("{line}");
        }
    }
}

/// Writes every event of `INFO` and above as a line: the time, the level, where it came from, the message and
/// the fields. Spans are not used by the Runner and are not kept.
pub struct Lines {
    sink: Mutex<Sink>,
}

impl Lines {
    /// A log in this file, rolled over at `roll_at` bytes.
    pub fn new(path: &Path, roll_at: u64) -> Lines {
        Lines {
            sink: Mutex::new(Sink {
                path: path.to_path_buf(),
                file: None,
                written: 0,
                roll_at,
            }),
        }
    }
}

struct Fields(String);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, " {value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        // One event is one line, whatever a value holds.
        let value = value.replace(['\n', '\r'], " ");
        if field.name() == "message" {
            let _ = write!(self.0, " {value}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

impl Subscriber for Lines {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= Level::INFO
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        let line = format!(
            "{} {:5} {}{}\n",
            crate::iso(seconds),
            event.metadata().level(),
            event.metadata().target(),
            fields.0.replace('\n', " ")
        );
        self.sink.lock().expect("log").line(&line);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_one_line_with_its_level_its_message_and_its_fields() {
        let folder = tempfile::tempdir().expect("folder");
        let path = folder.path().join("logs/runner.log");
        tracing::subscriber::with_default(Lines::new(&path, ROLL_AT), || {
            tracing::info!(runner = "mac-a-j1", waited = 3, "machine gone");
            tracing::debug!("not kept");
            tracing::warn!(error = %"two\nlines", "a listener failed");
        });
        let text = std::fs::read_to_string(&path).expect("log");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[0].ends_with(" machine gone runner=\"mac-a-j1\" waited=3"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains(" INFO  "), "{}", lines[0]);
        assert!(
            lines[0].starts_with("20") && lines[0].as_bytes()[10] == b'T',
            "{}",
            lines[0]
        );
        assert!(
            lines[1].ends_with(" a listener failed error=two lines"),
            "{}",
            lines[1]
        );
        assert!(lines[1].contains(" WARN  "), "{}", lines[1]);
    }

    #[test]
    fn a_full_log_is_moved_aside_once_and_begun_again() {
        let folder = tempfile::tempdir().expect("folder");
        let path = folder.path().join("runner.log");
        tracing::subscriber::with_default(Lines::new(&path, 200), || {
            for n in 0..10 {
                tracing::info!(n, "a line of some length to fill the file with");
            }
        });
        let now = std::fs::read_to_string(&path).expect("log");
        let before = std::fs::read_to_string(folder.path().join("runner.log.1")).expect("the one before");
        assert!(now.len() < 400 && !now.is_empty(), "{}", now.len());
        assert!(before.len() >= 200 && before.len() < 400, "{}", before.len());
        assert!(now.lines().last().expect("a line").contains("n=9"));
        let names: Vec<String> = std::fs::read_dir(folder.path())
            .expect("folder")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "one log and the one before: {names:?}");
    }
}
