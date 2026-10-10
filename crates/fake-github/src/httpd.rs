//! A small HTTP/1.1 server for tests. It speaks just enough of the protocol for `reqwest`, and it can misbehave on
//! purpose: stay silent, cut a response short, close the connection, answer late.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A request as the server read it.
#[derive(Debug, Clone)]
pub struct Request {
    /// `GET`, `POST` and so on.
    pub method: String,
    /// The path without the query.
    pub path: String,
    /// The decoded query pairs, in order.
    pub query: Vec<(String, String)>,
    /// Header names lowercased.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Request {
    /// A header's value, by lowercase name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// A query value.
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// `METHOD path`, as faults match it.
    pub fn line(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// What the server does with a request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// An ordinary response.
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Keep the connection open and never answer.
    Hang,
    /// Close the connection without a byte.
    Drop,
    /// Promise the whole body, send `sent` bytes of it, close.
    Truncated { status: u16, body: Vec<u8>, sent: usize },
    /// Wait, then do the inner reply.
    After(Duration, Box<Reply>),
}

impl Reply {
    /// A JSON response with status 200.
    pub fn json(value: &serde_json::Value) -> Reply {
        Reply::status_json(200, value)
    }

    /// A JSON response with the given status.
    pub fn status_json(status: u16, value: &serde_json::Value) -> Reply {
        Reply::Response {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(value).expect("json"),
        }
    }

    /// A plain text response.
    pub fn text(status: u16, text: &str) -> Reply {
        Reply::Response {
            status,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: text.as_bytes().to_vec(),
        }
    }

    /// An empty response.
    pub fn empty(status: u16) -> Reply {
        Reply::Response {
            status,
            headers: vec![],
            body: vec![],
        }
    }

    /// The same response with one more header.
    pub fn with_header(self, name: &str, value: &str) -> Reply {
        match self {
            Reply::Response {
                status,
                mut headers,
                body,
            } => {
                headers.push((name.to_lowercase(), value.to_string()));
                Reply::Response {
                    status,
                    headers,
                    body,
                }
            }
            Reply::After(d, inner) => Reply::After(d, Box::new(inner.with_header(name, value))),
            other => other,
        }
    }
}

/// What answers requests.
pub type Handler = Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Reply> + Send>> + Send + Sync>;

/// A running server. It stops when dropped.
pub struct Server {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Server {
    /// Starts a server on a free port of the loopback interface.
    pub async fn start(handler: Handler) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let _ = serve_connection(stream, handler).await;
                });
            }
        });
        Server { addr, task }
    }

    /// `http://127.0.0.1:port`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_connection(stream: TcpStream, handler: Handler) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    loop {
        let Some(request) = read_request(&mut read).await? else {
            return Ok(());
        };
        let keep_alive = request
            .header("connection")
            .map(|c| !c.eq_ignore_ascii_case("close"))
            .unwrap_or(true);
        let mut reply = handler(request).await;
        loop {
            match reply {
                Reply::After(wait, inner) => {
                    tokio::time::sleep(wait).await;
                    reply = *inner;
                }
                other => {
                    reply = other;
                    break;
                }
            }
        }
        match reply {
            Reply::Hang => {
                std::future::pending::<()>().await;
                return Ok(());
            }
            Reply::Drop => return Ok(()),
            Reply::Truncated { status, body, sent } => {
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                write.write_all(head.as_bytes()).await?;
                write.write_all(&body[..sent.min(body.len())]).await?;
                write.flush().await?;
                return Ok(());
            }
            Reply::Response {
                status,
                headers,
                body,
            } => {
                let mut head = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\n", body.len());
                for (k, v) in &headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                write.write_all(head.as_bytes()).await?;
                write.write_all(&body).await?;
                write.flush().await?;
                if !keep_alive {
                    return Ok(());
                }
            }
            Reply::After(..) => unreachable!(),
        }
    }
}

async fn read_request(
    read: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> std::io::Result<Option<Request>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if read.read(&mut byte).await? == 0 {
            return Ok(None);
        }
        head.push(byte[0]);
        if head.len() > 64 * 1024 {
            return Err(std::io::Error::other("head too large"));
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target, vec![]),
    };
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    let length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    read.read_exact(&mut body).await?;
    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}
