//! The socket a vm-host answers on: one request a connection, one line of JSON each way, the job's state back.

use crate::layout::Layout;
use crate::machine::Event;
use crate::state::JobState;
use cf_guest_proto::StopReason;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What can be asked of a vm-host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// The job's state as of now.
    State,
    /// End the job: tell it why, give it `grace_ms` to end by itself, then end it.
    Stop {
        /// Why.
        reason: StopReason,
        /// Milliseconds the job gets.
        grace_ms: u64,
    },
}

/// The longest request line a vm-host reads.
const MAX_LINE: u64 = 4096;

/// Asks the vm-host of `job` and returns the state it answers with. An error when no vm-host answers there,
/// which for a job whose state has no end means it is gone.
pub fn ask(layout: &Layout, job: &str, request: &Request, within: Duration) -> io::Result<JobState> {
    let mut stream = UnixStream::connect(layout.socket(job))?;
    stream.set_read_timeout(Some(within))?;
    stream.set_write_timeout(Some(within))?;
    let mut line = serde_json::to_string(request).map_err(io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream.take(1 << 20)).read_line(&mut answer)?;
    serde_json::from_str(&answer)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, answer.trim().to_string()))
}

/// Answers on the socket until `done`. A stop becomes an event of the job's; every well-formed request gets the
/// state as it was when it arrived.
pub(crate) fn serve(
    listener: UnixListener,
    state: Arc<Mutex<String>>,
    mut on_stop: impl FnMut(Event) + Send + 'static,
    done: Arc<AtomicBool>,
) {
    let _ = listener.set_nonblocking(true);
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            // One slow or silent client holds the socket for a second at most.
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let mut line = String::new();
            let read = BufReader::new((&stream).take(MAX_LINE)).read_line(&mut line);
            let answer = match (read, serde_json::from_str::<Request>(line.trim())) {
                (Ok(_), Ok(request)) => {
                    let now = state.lock().map(|state| state.clone()).unwrap_or_default();
                    if let Request::Stop { reason, grace_ms } = request {
                        on_stop(Event::StopAsked {
                            reason,
                            grace: Duration::from_millis(grace_ms),
                        });
                    }
                    now
                }
                _ => r#"{"error":"not a request"}"#.to_string(),
            };
            let _ = (&stream).write_all(format!("{answer}\n").as_bytes());
        }
    });
}
