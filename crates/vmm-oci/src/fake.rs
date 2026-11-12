//! An engine for the unit tests: a Unix socket that answers each request with what a test's function says, one
//! request a connection, and keeps every request it was asked.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::sync::{Arc, Mutex};

/// One request as the engine read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

impl Asked {
    /// `METHOD path`, as the tests compare them.
    pub fn line(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// The engine's socket and what it was asked.
pub struct FakeEngine {
    _folder: tempfile::TempDir,
    /// Its socket.
    pub socket: std::path::PathBuf,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl FakeEngine {
    /// An engine whose answer to each request is `answer(request)`: a status and a JSON body.
    pub fn new(answer: impl Fn(&Asked) -> (u16, String) + Send + 'static) -> Self {
        let folder = tempfile::tempdir().expect("a folder");
        let socket = folder.path().join("engine.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("the socket");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let kept = asked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(stream);
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let mut parts = first.split_whitespace();
                let request = Asked {
                    method: parts.next().unwrap_or_default().into(),
                    path: parts.next().unwrap_or_default().into(),
                    body,
                };
                let (status, text) = answer(&request);
                kept.lock().expect("asked").push(request);
                let _ = write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        Self {
            socket,
            _folder: folder,
            asked,
        }
    }

    /// Every request so far, in order.
    pub fn asked(&self) -> Vec<Asked> {
        self.asked.lock().expect("asked").clone()
    }

    /// The `METHOD path` of every request so far that used this method.
    pub fn lines(&self, method: &str) -> Vec<String> {
        self.asked()
            .iter()
            .filter(|asked| asked.method == method)
            .map(Asked::line)
            .collect()
    }
}
