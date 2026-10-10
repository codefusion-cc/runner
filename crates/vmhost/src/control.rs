//! The socket a vm-host answers on: one request a connection, one line of JSON each way, the job's state back.

use crate::layout::Layout;
use crate::machine::Event;
use crate::state::JobState;
use cf_guest_proto::StopReason;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
const MAX_LINE: usize = 4096;
/// What one connection gets from its first byte to its answer, and the answer to be taken. The socket answers one
/// client at a time, so this is the longest a slow or silent one keeps the daemon waiting.
const CONNECTION: Duration = Duration::from_secs(1);

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

/// Whether the vm-host that wrote `state` is still there: its socket answers within `within`, for this job and
/// from the same process. This, not the process id, tells a running job from a lost one: once a vm-host is gone
/// its number may be another process's.
pub fn answers(layout: &Layout, state: &JobState, within: Duration) -> bool {
    ask(layout, &state.job, &Request::State, within)
        .is_ok_and(|now| now.job == state.job && now.pid == state.pid)
}

/// One request line, read within `within` however slowly its bytes come. `None` for what is no line of text, is
/// longer than [`MAX_LINE`] or did not arrive in time.
fn request_line(mut stream: &UnixStream, within: Duration) -> Option<String> {
    let until = Instant::now() + within;
    let mut line = Vec::new();
    let mut buffer = [0u8; 512];
    while !line.contains(&b'\n') {
        let left = until.checked_duration_since(Instant::now())?;
        if left.is_zero() || line.len() > MAX_LINE {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => line.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    let line = line.split(|byte| *byte == b'\n').next()?;
    String::from_utf8(line.to_vec()).ok()
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
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_write_timeout(Some(CONNECTION));
            let request = request_line(&stream, CONNECTION)
                .and_then(|line| serde_json::from_str::<Request>(line.trim()).ok());
            let answer = match request {
                Some(request) => {
                    let now = state.lock().map(|state| state.clone()).unwrap_or_default();
                    if let Request::Stop { reason, grace_ms } = request {
                        on_stop(Event::StopAsked {
                            reason,
                            grace: Duration::from_millis(grace_ms),
                        });
                    }
                    now
                }
                None => r#"{"error":"not a request"}"#.to_string(),
            };
            let _ = (&stream).write_all(format!("{answer}\n").as_bytes());
        }
    });
}
