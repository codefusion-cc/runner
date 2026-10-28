//! The trusted class's gate: what stands in for GitHub's `Runner.Worker` on a trusted machine. The Actions runner's
//! listener starts its Worker as `Runner.Worker spawnclient <in> <out>` and sends the job message down the pipe `in`:
//! a 32-bit little-endian message type, a 32-bit little-endian length in bytes, then the message as UTF-16LE text
//! (the Actions runner's `ProcessChannel` and `StreamString`). The gate reads that one message, lets the job run only
//! when [`cf_trusted::decide`] accepts it, and otherwise ends before the Worker starts, so no step of the job runs: the
//! listener fails the job with what the gate printed as its error.
//!
//! What the gate reads is GitHub's message through the Actions runner, before anything of the job has run in the
//! machine; nothing in the checked-out repository or the job's environment reaches it.

use std::io::Read;

use cf_trusted::{Accepted, Policy};
use serde::{Deserialize, Serialize};

/// The message type of a new job (`MessageType.NewJobRequest`).
pub const NEW_JOB_REQUEST: i32 = 1;
/// The largest message the Actions runner's channel carries (`StreamString.MaxStringSize`), in bytes.
pub const MAX_MESSAGE_BYTES: usize = 50_000_000;
/// The gate's file in the shared folder: the rules, the Worker it starts and where it leaves its verdict.
pub const CONFIG: &str = "gate.json";
/// The name the Worker of the image is moved to, beside the gate that takes its place.
pub const REAL_WORKER: &str = "Runner.Worker.real";
/// The verdict's file in the job's home, which the supervisor reads back for the machine's log.
pub const VERDICT: &str = ".codefusion-gate";
/// The longest verdict the supervisor reads back.
pub const VERDICT_MAX: usize = 2048;

/// What the supervisor tells the gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateConfig {
    /// The trusted class's rules.
    pub policy: Policy,
    /// The image's own Worker, moved aside.
    pub worker: String,
    /// Where the verdict goes.
    pub verdict: String,
}

/// The first message on the Worker's channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The message's bytes as they came, header and all, to hand on to the Worker unchanged.
    pub frame: Vec<u8>,
    /// Its type.
    pub kind: i32,
    /// Its text, or why it is not UTF-16.
    pub text: Result<String, String>,
}

/// Reads the first message from the listener. An error when the channel ends early or announces more than the
/// Actions runner itself would send.
pub fn read_request(input: &mut impl Read) -> Result<Request, String> {
    let mut header = [0u8; 8];
    input
        .read_exact(&mut header)
        .map_err(|error| format!("the job message did not come ({error})"))?;
    let kind = i32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let length = i32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let length = usize::try_from(length)
        .ok()
        .filter(|length| *length <= MAX_MESSAGE_BYTES)
        .ok_or_else(|| format!("the job message announces {length} bytes"))?;
    let mut frame = Vec::with_capacity(8 + length);
    frame.extend_from_slice(&header);
    input
        .take(length as u64)
        .read_to_end(&mut frame)
        .map_err(|error| format!("the job message was cut off ({error})"))?;
    if frame.len() != 8 + length {
        return Err(format!(
            "the job message was cut off after {} of {length} bytes",
            frame.len() - 8
        ));
    }
    let text = utf16(&frame[8..]);
    Ok(Request { frame, kind, text })
}

/// UTF-16LE text, refusing an odd length or a half of a pair.
fn utf16(bytes: &[u8]) -> Result<String, String> {
    if !bytes.len().is_multiple_of(2) {
        return Err("the job message is not UTF-16 (an odd number of bytes)".into());
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| "the job message is not UTF-16 (half of a surrogate pair)".into())
}

/// Whether the job of `request` may run.
pub fn decide(policy: &Policy, request: &Request) -> Result<Accepted, String> {
    if request.kind != NEW_JOB_REQUEST {
        return Err(format!(
            "the first message to the Worker is of type {}, not a new job",
            request.kind
        ));
    }
    let text = request.text.as_ref().map_err(Clone::clone)?;
    cf_trusted::decide(policy, text)
}

/// The line a refused job ends with, as its error on GitHub.
pub fn refusal(why: &str) -> String {
    format!(
        "codefusion-runner: this trusted machine runs only main's deploy, release and signing jobs, and refuses this \
         one before any step: {why}."
    )
}

/// What the verdict file says: one line of at most [`VERDICT_MAX`] bytes.
pub fn verdict_line(verdict: &Result<Accepted, String>) -> String {
    let line = match verdict {
        Ok(accepted) => format!("accepted {accepted}"),
        Err(why) => format!("refused: {why}"),
    };
    clip(&line)
}

/// What the supervisor says of a verdict file it read back, which an accepted job could have written over: its
/// first line, without control characters, and at most [`VERDICT_MAX`] bytes.
pub fn read_back(text: &str) -> String {
    clip(text.lines().next().unwrap_or_default())
}

fn clip(text: &str) -> String {
    let mut line = String::new();
    for c in text.chars().map(|c| if c.is_control() { ' ' } else { c }) {
        if line.len() + c.len_utf8() > VERDICT_MAX {
            break;
        }
        line.push(c);
    }
    line
}
