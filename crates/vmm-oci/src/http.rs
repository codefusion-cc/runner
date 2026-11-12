//! HTTP/1.1 to an engine's Unix socket: one connection a request, closed after the answer, and the attach that turns
//! a connection into the container's standard streams. Only what the Docker Engine API needs: no TLS, no redirects,
//! no keep-alive.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The most of an answer's head this reads: an engine's are a few hundred bytes.
const HEAD_MAX: usize = 64 << 10;
/// The most of an answer's body this keeps: the largest the Runner reads is a container's inspection.
pub const BODY_MAX: usize = 16 << 20;

/// An engine's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The body, whole.
    pub body: Vec<u8>,
}

impl Response {
    /// The body as text, for an error's message: the engine says why in `{"message": ...}`.
    pub fn message(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|value| value.get("message")?.as_str().map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body[..self.body.len().min(512)]).into_owned())
    }

    /// The body as JSON.
    pub fn json(&self) -> io::Result<serde_json::Value> {
        serde_json::from_slice(&self.body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// Whether the status is 2xx.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A request's body.
pub enum Body<'a> {
    /// None.
    Empty,
    /// JSON.
    Json(&'a serde_json::Value),
    /// A tar archive.
    Tar(&'a [u8]),
}

/// The engine at a Unix socket.
#[derive(Debug, Clone)]
pub struct Engine {
    socket: PathBuf,
}

impl Engine {
    /// The engine listening at this socket.
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
        }
    }

    fn connect(&self, timeout: Option<Duration>) -> io::Result<UnixStream> {
        let stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        Ok(stream)
    }

    /// Sends one request and reads its whole answer; `timeout` bounds every read, `None` waits as long as the
    /// engine takes (a container's end).
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Body<'_>,
        timeout: Option<Duration>,
    ) -> io::Result<Response> {
        let mut stream = self.connect(timeout)?;
        send(&mut stream, method, path, &body, &[])?;
        let mut reader = BufReader::new(stream);
        let (status, headers) = read_head(&mut reader)?;
        let body = read_body(&mut reader, &headers)?;
        Ok(Response { status, body })
    }

    /// Sends `length` bytes of `body` as a request of this content type, read and sent as they come, and reads the
    /// whole answer. A body that ends before `length` is an error, and the engine sees the request cut short.
    pub fn upload(
        &self,
        path: &str,
        kind: &str,
        body: &mut dyn io::Read,
        length: u64,
        timeout: Option<Duration>,
    ) -> io::Result<Response> {
        let mut stream = self.connect(timeout)?;
        send_head(&mut stream, "POST", path, Some(kind), length, &[])?;
        // Sent a MiB at a time: the copy reads into the writer's buffer, at its size.
        let mut sending = io::BufWriter::with_capacity(1 << 20, stream);
        let sent = io::copy(&mut io::Read::take(body, length), &mut sending)?;
        if sent != length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("the body ended after {sent} of {length} bytes"),
            ));
        }
        let mut stream = sending.into_inner().map_err(io::IntoInnerError::into_error)?;
        stream.flush()?;
        let mut reader = BufReader::new(stream);
        let (status, headers) = read_head(&mut reader)?;
        let body = read_body(&mut reader, &headers)?;
        Ok(Response { status, body })
    }

    /// Attaches to a container's standard input, output and error. The answer is the connection itself, raw from
    /// here on, with what the engine sent of the streams with its head.
    pub fn attach(&self, path: &str) -> io::Result<(UnixStream, Vec<u8>)> {
        let mut stream = self.connect(Some(Duration::from_secs(30)))?;
        send(
            &mut stream,
            "POST",
            path,
            &Body::Empty,
            &[("Connection", "Upgrade"), ("Upgrade", "tcp")],
        )?;
        let mut reader = BufReader::new(stream);
        let (status, headers) = read_head(&mut reader)?;
        // Docker switches protocols; Podman and older engines answer 200 and stream on the same connection.
        if status != 101 && status != 200 {
            let body = read_body(&mut reader, &headers).unwrap_or_default();
            let said = Response { status, body }.message();
            return Err(io::Error::other(format!(
                "the attach was refused ({status}): {said}"
            )));
        }
        let early = reader.buffer().to_vec();
        let stream = reader.into_inner();
        stream.set_read_timeout(None)?;
        Ok((stream, early))
    }
}

fn send(
    stream: &mut UnixStream,
    method: &str,
    path: &str,
    body: &Body<'_>,
    extra: &[(&str, &str)],
) -> io::Result<()> {
    let json;
    let (kind, bytes): (Option<&str>, &[u8]) = match body {
        Body::Empty => (None, &[]),
        Body::Json(value) => {
            json = serde_json::to_vec(value)?;
            (Some("application/json"), &json)
        }
        Body::Tar(bytes) => (Some("application/x-tar"), bytes),
    };
    send_head(stream, method, path, kind, bytes.len() as u64, extra)?;
    stream.write_all(bytes)?;
    stream.flush()
}

fn send_head(
    stream: &mut UnixStream,
    method: &str,
    path: &str,
    kind: Option<&str>,
    length: u64,
    extra: &[(&str, &str)],
) -> io::Result<()> {
    if !path.starts_with('/') || path.bytes().any(|byte| byte.is_ascii_control() || byte == b' ') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no path for a request",
        ));
    }
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: engine\r\nUser-Agent: codefusion-runner\r\n");
    if let Some(kind) = kind {
        head.push_str(&format!("Content-Type: {kind}\r\n"));
    }
    head.push_str(&format!("Content-Length: {length}\r\n"));
    if extra.is_empty() {
        head.push_str("Connection: close\r\n");
    }
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

fn bad(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

/// The status and the headers (names in lowercase) of an answer.
pub fn read_head(reader: &mut impl BufRead) -> io::Result<(u16, Vec<(String, String)>)> {
    let mut read = 0usize;
    let mut line = String::new();
    let mut next = |line: &mut String| -> io::Result<()> {
        line.clear();
        let taken = reader.take((HEAD_MAX - read) as u64).read_line(line)?;
        read += taken;
        if taken == 0 || !line.ends_with('\n') {
            return Err(bad("the engine's answer ended in its head"));
        }
        Ok(())
    };
    next(&mut line)?;
    let status = line
        .strip_prefix("HTTP/1.")
        .and_then(|rest| rest.get(2..5))
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|code| (100..600).contains(code))
        .ok_or_else(|| bad(format!("no status line: {:?}", line.trim_end())))?;
    let mut headers = Vec::new();
    loop {
        next(&mut line)?;
        let text = line.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            return Ok((status, headers));
        }
        let (name, value) = text
            .split_once(':')
            .ok_or_else(|| bad("a header without a colon"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(have, _)| have == name)
        .map(|(_, value)| value.as_str())
}

/// The body after a head: by its length, in chunks, or to the connection's end; [`BODY_MAX`] at most.
pub fn read_body(reader: &mut impl BufRead, headers: &[(String, String)]) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if header(headers, "transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut line = String::new();
            reader.take(1024).read_line(&mut line)?;
            let size = line.trim_end().split(';').next().unwrap_or_default();
            let size = usize::from_str_radix(size.trim(), 16).map_err(|_| bad("a chunk without its size"))?;
            if size == 0 {
                // The trailer, if any, up to the empty line.
                loop {
                    line.clear();
                    if reader.take(1024).read_line(&mut line)? == 0 || line.trim_end().is_empty() {
                        return Ok(body);
                    }
                }
            }
            // A size near the largest number would wrap the sum past the check.
            if body.len().checked_add(size).is_none_or(|total| total > BODY_MAX) {
                return Err(bad("the engine's answer is too large"));
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..])?;
            let mut end = [0u8; 2];
            reader.read_exact(&mut end)?;
            if &end != b"\r\n" {
                return Err(bad("a chunk that does not end its line"));
            }
        }
    }
    match header(headers, "content-length") {
        Some(length) => {
            let length: usize = length.parse().map_err(|_| bad("a length that is no number"))?;
            if length > BODY_MAX {
                return Err(bad("the engine's answer is too large"));
            }
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        }
        None => {
            reader.take(BODY_MAX as u64 + 1).read_to_end(&mut body)?;
            if body.len() > BODY_MAX {
                return Err(bad("the engine's answer is too large"));
            }
        }
    }
    Ok(body)
}

/// A query's value, with everything but unreserved characters escaped.
pub fn escape(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn answer(text: &[u8]) -> io::Result<(u16, Vec<u8>)> {
        let mut reader = Cursor::new(text.to_vec());
        let (status, headers) = read_head(&mut reader)?;
        Ok((status, read_body(&mut reader, &headers)?))
    }

    #[test]
    fn a_body_is_read_by_length_in_chunks_or_to_the_end() {
        assert_eq!(
            answer(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello and more").unwrap(),
            (200, b"hello".to_vec())
        );
        assert_eq!(
            answer(b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\nT: v\r\n\r\n")
                .unwrap(),
            (201, b"abcde".to_vec())
        );
        assert_eq!(
            answer(b"HTTP/1.0 404 Not Found\r\n\r\n{\"message\":\"no such container\"}").unwrap(),
            (404, b"{\"message\":\"no such container\"}".to_vec())
        );
        assert_eq!(
            answer(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").unwrap(),
            (204, Vec::new())
        );
    }

    #[test]
    fn an_answer_that_is_cut_short_or_malformed_is_an_error_not_a_guess() {
        for text in [
            &b""[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel",
            b"HTTP/1.1 OK\r\n\r\n",
            b"SSH-2.0-OpenSSH\r\n\r\n",
            b"HTTP/1.1 999 What\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: lots\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXX0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999\r\n\r\n",
        ] {
            assert!(answer(text).is_err(), "{:?}", String::from_utf8_lossy(text));
        }
        let huge = format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "a".repeat(HEAD_MAX));
        assert!(
            answer(huge.as_bytes()).is_err(),
            "a head without end is not read for ever"
        );
    }

    #[test]
    fn the_engines_message_is_what_an_error_says() {
        let refused = Response {
            status: 409,
            body: br#"{"message":"container is not running"}"#.to_vec(),
        };
        assert_eq!(refused.message(), "container is not running");
        assert!(!refused.ok());
        let plain = Response {
            status: 500,
            body: b"oops".to_vec(),
        };
        assert_eq!(plain.message(), "oops");
    }

    #[test]
    fn a_query_value_is_escaped_whole() {
        assert_eq!(
            escape("label=cc.codefusion/x y&z"),
            "label%3Dcc.codefusion%2Fx%20y%26z"
        );
        assert_eq!(escape("plain-1.2_3~"), "plain-1.2_3~");
        assert_eq!(escape("ż"), "%C5%BC");
    }

    /// A chunk size near usize::MAX after a first chunk wraps `body.len() + size`
    /// past the BODY_MAX check (a panic in a debug build, a slice out of range in release), where an error is due.
    #[test]
    fn a_chunk_size_that_overflows_is_an_error_not_a_panic() {
        let text = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\nffffffffffffffff\r\n";
        let result = std::panic::catch_unwind(|| answer(text).is_err());
        assert_eq!(
            result.ok(),
            Some(true),
            "the vm-host panicked on the engine's answer"
        );
    }

    /// A request's head and body, as the engine read them.
    type Received = (String, Vec<u8>);

    /// An engine at a socket of its own that reads one request, head and `Content-Length` bytes, answers 200 and
    /// gives back what it read: `None` when the request was cut short.
    fn one_request_engine() -> (
        tempfile::TempDir,
        Engine,
        std::thread::JoinHandle<Option<Received>>,
    ) {
        let folder = tempfile::tempdir().unwrap();
        let socket = folder.path().join("engine.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let served = std::thread::spawn(move || {
            let (stream, _) = listener.accept().ok()?;
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).ok()?;
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))?
                .trim()
                .parse()
                .ok()?;
            let mut body = vec![0; length];
            reader.read_exact(&mut body).ok()?;
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .ok()?;
            Some((head, body))
        });
        (folder, Engine::new(&socket), served)
    }

    #[test]
    fn an_upload_sends_its_body_as_it_is_read_and_one_that_ends_early_is_cut_short() {
        let (_folder, engine, served) = one_request_engine();
        let body: Vec<u8> = (0..3_000_000u32).map(|n| n as u8).collect();
        let answer = engine
            .upload(
                "/images/create?fromSrc=-",
                "application/x-tar",
                &mut &body[..],
                body.len() as u64,
                None,
            )
            .unwrap();
        assert_eq!((answer.status, answer.body.as_slice()), (200, &b"ok"[..]));
        let (head, received) = served.join().unwrap().expect("the whole request");
        assert!(
            head.starts_with("POST /images/create?fromSrc=- HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("Content-Type: application/x-tar\r\n"), "{head}");
        assert!(received == body, "every byte, in order");

        let (_folder, engine, served) = one_request_engine();
        let said = engine
            .upload(
                "/images/create?fromSrc=-",
                "application/x-tar",
                &mut &body[..1000],
                5000,
                Some(Duration::from_secs(5)),
            )
            .unwrap_err();
        assert_eq!(said.kind(), io::ErrorKind::UnexpectedEof, "{said}");
        assert_eq!(
            served.join().unwrap(),
            None,
            "the engine never got a whole request"
        );
    }
}
