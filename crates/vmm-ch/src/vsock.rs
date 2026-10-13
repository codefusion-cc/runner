//! The host's side of a guest's vsock port under cloud-hypervisor: a Unix socket, on which the host writes
//! `CONNECT <port>\n` and the monitor answers `OK <host port>\n` once the guest took the connection. After that
//! line the socket carries the guest's bytes.
//!
//! The answer is read byte by byte, so not one byte of the guest's own is taken with it, and within a deadline:
//! a monitor or guest that says something else, too much, or nothing at all costs one failed attempt, never a
//! thread of the host.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// How long the whole handshake may take.
pub const HANDSHAKE: Duration = Duration::from_secs(5);

/// How long a write to the guest may wait for the guest to read, as under Virtualization.framework.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest answer read: `OK ` and a port number with room to spare.
const MAX_ANSWER: usize = 32;

/// Whether a line (without its newline) is the monitor's yes: `OK `, then a port number.
pub fn is_yes(line: &[u8]) -> bool {
    let Some(port) = line.strip_prefix(b"OK ") else {
        return false;
    };
    !port.is_empty()
        && port.len() <= 10
        && port.iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(port).is_ok_and(|port| port.parse::<u32>().is_ok())
}

/// Asks the monitor on `stream` for the guest's `port`, within `within`. On success the stream is the guest's
/// connection: reads wait as long as the guest is silent, writes at most [`SEND_TIMEOUT`].
pub fn handshake(stream: &UnixStream, port: u32, within: Duration) -> Result<(), String> {
    let deadline = Instant::now() + within;
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| format!("no answer within {within:?}"))
    };
    let mut writer = stream;
    stream
        .set_write_timeout(Some(left()?))
        .map_err(|e| e.to_string())?;
    writer
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| format!("asking: {e}"))?;
    let mut answer = Vec::with_capacity(MAX_ANSWER);
    let mut reader = stream;
    loop {
        stream
            .set_read_timeout(Some(left()?))
            .map_err(|e| e.to_string())?;
        let mut byte = [0u8];
        match reader.read(&mut byte) {
            Ok(0) => return Err(refused("the monitor closed the connection", &answer)),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if answer.len() == MAX_ANSWER => return Err(refused("the answer is too long", &answer)),
            Ok(_) => answer.push(byte[0]),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(refused(&format!("no answer within {within:?}"), &answer));
            }
            Err(error) => return Err(refused(&error.to_string(), &answer)),
        }
    }
    if !is_yes(&answer) {
        return Err(refused("the monitor said no", &answer));
    }
    stream.set_read_timeout(None).map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(SEND_TIMEOUT))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn refused(why: &str, answer: &[u8]) -> String {
    if answer.is_empty() {
        return why.to_string();
    }
    format!("{why} ({:?})", String::from_utf8_lossy(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::io::BufRead as _;
    use std::thread;

    /// A monitor that reads the request, answers `reply`, and then hands the socket to `then`.
    fn monitor(reply: &'static [u8], then: impl FnOnce(UnixStream) + Send + 'static) -> UnixStream {
        let (host, monitor) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let mut request = String::new();
            std::io::BufReader::new(&monitor).read_line(&mut request).unwrap();
            assert_eq!(request, "CONNECT 1024\n");
            let mut writer = &monitor;
            let _ = writer.write_all(reply);
            then(monitor);
        });
        host
    }

    /// Keeps the monitor's end open a while, as a monitor does: macOS refuses socket options once the peer closed.
    fn hold(socket: UnixStream) {
        thread::sleep(Duration::from_millis(300));
        drop(socket);
    }

    #[test]
    fn a_yes_hands_over_the_guest_with_its_first_bytes() {
        let host = monitor(b"OK 1073741824\nfrom the guest", hold);
        handshake(&host, 1024, HANDSHAKE).unwrap();
        let mut said = String::new();
        (&host).read_to_string(&mut said).unwrap();
        assert_eq!(said, "from the guest");
        assert_eq!(host.read_timeout().unwrap(), None, "a silent guest is no error");
        assert_eq!(host.write_timeout().unwrap(), Some(SEND_TIMEOUT));
    }

    #[test]
    fn a_closed_connection_is_refused() {
        let host = monitor(b"", drop);
        let why = handshake(&host, 1024, HANDSHAKE).unwrap_err();
        assert!(why.contains("closed"), "{why}");
    }

    #[test]
    fn anything_but_a_yes_is_refused() {
        for reply in [
            &b"ERR no listener\n"[..],
            b"OK\n",
            b"OK \n",
            b"OK 12a\n",
            b"ok 12\n",
            b"OK 99999999999\n",
            b" OK 1\n",
        ] {
            let host = monitor(Box::leak(reply.to_vec().into_boxed_slice()), hold);
            assert!(
                handshake(&host, 1024, HANDSHAKE).is_err(),
                "{:?} was taken",
                String::from_utf8_lossy(reply)
            );
        }
    }

    #[test]
    fn a_long_answer_is_refused_without_reading_on() {
        let host = monitor(
            b"OK 0000000000000000000000000000000000000000000000000000000\n",
            hold,
        );
        let why = handshake(&host, 1024, HANDSHAKE).unwrap_err();
        assert!(why.contains("too long"), "{why}");
    }

    #[test]
    fn a_silent_monitor_costs_the_deadline_and_no_more() {
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let host = monitor(b"", move |socket| {
            let _ = wait.recv();
            drop(socket);
        });
        let began = Instant::now();
        let why = handshake(&host, 1024, Duration::from_millis(300)).unwrap_err();
        assert!(why.contains("no answer"), "{why}");
        assert!(began.elapsed() >= Duration::from_millis(300));
        assert!(began.elapsed() < Duration::from_secs(3), "{:?}", began.elapsed());
        drop(done);
    }

    #[test]
    fn a_monitor_that_dribbles_bytes_is_held_to_one_deadline() {
        let host = monitor(b"", |socket| {
            let mut writer = &socket;
            for _ in 0..20 {
                if writer.write_all(b"O").is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        let began = Instant::now();
        assert!(handshake(&host, 1024, Duration::from_millis(400)).is_err());
        assert!(
            began.elapsed() < Duration::from_millis(900),
            "{:?}",
            began.elapsed()
        );
    }

    #[test]
    fn yes_is_ok_and_a_port() {
        assert!(is_yes(b"OK 0"));
        assert!(is_yes(b"OK 4294967295"));
        assert!(!is_yes(b"OK 4294967296"));
        assert!(!is_yes(b"OK -1"));
        assert!(!is_yes(b"OK 1 "));
        assert!(!is_yes(b""));
    }

    proptest! {
        #[test]
        fn yes_is_exactly_ok_and_a_number(port: u32, noise in proptest::collection::vec(any::<u8>(), 0..40)) {
            let yes = format!("OK {port}");
            prop_assert!(is_yes(yes.as_bytes()));
            let reads_as_yes = std::str::from_utf8(&noise)
                .ok()
                .and_then(|text| text.strip_prefix("OK "))
                .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_digit()) && digits.len() <= 10 && digits.parse::<u32>().is_ok());
            prop_assert_eq!(is_yes(&noise), reads_as_yes);
        }
    }
}
