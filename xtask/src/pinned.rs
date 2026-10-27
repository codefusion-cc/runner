//! Files a build reads from the network, pinned by their size and SHA-256: the guest's kernel package
//! (`kernel.rs`) and the Linux monitor (`cloud_hypervisor.rs`).
//!
//! A pinned file is read from our copy first, a release of this repository, and from its project only when the
//! copy is missing. Whichever answers, the bytes must have the pinned size and SHA-256: HTTPS only carries them. A
//! weekly check of each pin goes red when our copy stops answering with the pinned bytes, and warns when the
//! project has something newer or does not answer.

use std::fmt;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::Result;

/// What a download must be.
#[derive(Clone, Copy, Debug)]
pub struct Pin<'a> {
    pub size: u64,
    pub sha256: &'a str,
}

impl Pin<'_> {
    /// Whether `bytes` are exactly the pinned ones.
    pub fn check(&self, bytes: &[u8]) -> std::result::Result<(), FetchError> {
        if bytes.len() as u64 != self.size {
            return Err(FetchError::Size(bytes.len() as u64));
        }
        let hash = format!("{:x}", Sha256::digest(bytes));
        if hash != self.sha256 {
            return Err(FetchError::Hash(hash));
        }
        Ok(())
    }
}

/// A place a file can be read from, and the hosts its answer may come from after redirects.
#[derive(Clone, Debug)]
pub struct Source {
    pub name: &'static str,
    pub url: String,
    pub hosts: &'static [&'static str],
}

/// The hosts a GitHub release asset answers from: github.com redirects to its storage.
pub const GITHUB_RELEASE_HOSTS: &[&str] = &[
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// How a download goes over the network: curl, HTTPS only, one request per hop with a deadline for the whole.
pub struct Curl {
    /// curl's `--proto`.
    pub(crate) protocols: &'static str,
    /// More arguments, for tests (`--resolve`).
    pub(crate) extra: Vec<String>,
    /// How long one connection may take to open.
    pub(crate) connect: Duration,
    /// How long a pinned file may take, every hop of it.
    pub(crate) file: Duration,
    /// How long a listing of what is newest may take, every hop of it.
    pub(crate) listing: Duration,
}

impl Curl {
    pub fn https() -> Self {
        Curl {
            protocols: "=https",
            extra: Vec::new(),
            connect: Duration::from_secs(15),
            file: Duration::from_secs(300),
            listing: Duration::from_secs(60),
        }
    }
}

/// `duration` as the seconds curl is given, rounded down to its millisecond, or `None` below one millisecond: curl
/// reads 0 as no limit at all, so what rounds to it must not become a request.
fn curl_seconds(duration: Duration) -> Option<String> {
    let millis = duration.as_millis();
    (millis > 0).then(|| format!("{}.{:03}", millis / 1000, millis % 1000))
}

/// The most redirects a download follows.
const MAX_REDIRECTS: usize = 5;
/// The largest listing read.
pub const MAX_LISTING_BYTES: u64 = 16 << 20;

/// Why a source did not give the pinned bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// No answer, an HTTP error status (404 among them), a body cut off or too long, too many redirects, a deadline.
    Transport(String),
    /// A hop would go to a host the source does not name. Nothing was asked of it.
    Host(String),
    /// The body has another length than the pin.
    Size(u64),
    /// The body has another SHA-256 than the pin.
    Hash(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Transport(why) => write!(f, "{why}"),
            FetchError::Host(host) => write!(
                f,
                "it sends the request to {host}, which the source does not name"
            ),
            FetchError::Size(size) => write!(f, "the file has {size} bytes, not the pinned size"),
            FetchError::Hash(hash) => write!(f, "the file has SHA-256 {hash}, not the pinned one"),
        }
    }
}

/// Downloads `source` to `dest` if it gives exactly the pinned bytes; leaves no file at `dest` otherwise.
pub fn download(
    curl: &Curl,
    source: &Source,
    pin: Pin<'_>,
    dest: &Path,
) -> std::result::Result<(), FetchError> {
    let partial = dest.with_extension("partial");
    let result = get(curl, source, pin.size, curl.file, &partial)
        .and_then(|()| fs::read(&partial).map_err(|e| FetchError::Transport(e.to_string())))
        .and_then(|bytes| pin.check(&bytes))
        .and_then(|()| fs::rename(&partial, dest).map_err(|e| FetchError::Transport(e.to_string())));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// Asks `source` for its body, at most `max_bytes` of it, into `out`, within `deadline` for every hop together.
/// Redirects are followed here, not by curl, so each hop's host is checked before its request leaves.
pub fn get(
    curl: &Curl,
    source: &Source,
    max_bytes: u64,
    deadline: Duration,
    out: &Path,
) -> std::result::Result<(), FetchError> {
    let started = Instant::now();
    let mut url = source.url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let host = host_of(&url).unwrap_or_default();
        if !source.hosts.contains(&host.as_str()) {
            return Err(FetchError::Host(host));
        }
        let left = deadline.saturating_sub(started.elapsed());
        // The connection gets its own limit, never more than what is left of the whole.
        let (Some(total), Some(connect)) = (curl_seconds(left), curl_seconds(curl.connect.min(left))) else {
            return Err(FetchError::Transport(format!(
                "no answer within {} s",
                deadline.as_secs()
            )));
        };
        let output = Command::new("curl")
            .args(["-sS", "--proto", curl.protocols])
            .args(["--connect-timeout", &connect])
            .args(["--max-time", &total])
            // One byte more than allowed, so a longer body is cut off and refused, not stored whole.
            .args(["--max-filesize", &(max_bytes + 1).to_string()])
            .args(["-w", "%{http_code} %{redirect_url}", "-o"])
            .arg(out)
            .args(&curl.extra)
            .arg(&url)
            .output()
            .map_err(|e| FetchError::Transport(format!("curl could not start: {e}")))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            return Err(FetchError::Transport(format!(
                "curl failed with {}: {}",
                output.status,
                said.trim()
            )));
        }
        let written = String::from_utf8_lossy(&output.stdout);
        let (status, location) = written.split_once(' ').unwrap_or((&written, ""));
        match status {
            "200" => return Ok(()),
            "301" | "302" | "303" | "307" | "308" if !location.is_empty() => {
                url = location.trim().to_string()
            }
            _ => return Err(FetchError::Transport(format!("the answer was HTTP {status}"))),
        }
    }
    Err(FetchError::Transport(format!(
        "more than {MAX_REDIRECTS} redirects"
    )))
}

/// Tries each source in order and keeps the first that gives the pinned bytes; says what each one before it did.
/// `what` names the file in the error.
pub fn fetch(curl: &Curl, sources: &[Source], pin: Pin<'_>, dest: &Path, what: &str) -> Result<()> {
    let mut failures = Vec::new();
    for source in sources {
        match download(curl, source, pin, dest) {
            Ok(()) => {
                for failure in &failures {
                    eprintln!("warning: {failure}");
                }
                return Ok(());
            }
            Err(error) => failures.push(format!("{} ({}): {error}", source.name, source.url)),
        }
    }
    Err(format!("no source gave the pinned {what}:\n  {}", failures.join("\n  ")).into())
}

/// `dest` as it is when it holds the pinned bytes, else fetched anew.
pub fn ensure(curl: &Curl, sources: &[Source], pin: Pin<'_>, dest: &Path, what: &str) -> Result<()> {
    if fs::read(dest).is_ok_and(|bytes| pin.check(&bytes).is_ok()) {
        return Ok(());
    }
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fetch(curl, sources, pin, dest, what)
}

/// The host of a URL, lower case, without user, port or path.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next()?
    } else {
        host_port.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// What a check found: whether our copy (the first source) gives the pin, and what else is worth a look.
#[derive(Debug)]
pub struct Checked {
    pub copy: std::result::Result<(), String>,
    pub notices: Vec<String>,
}

/// Asks every source for the pinned bytes, into `dir`. Only the first, our copy, can fail the check: a build would
/// otherwise live on a URL its project may delete. The others failing is a notice.
pub fn check_sources(curl: &Curl, sources: &[Source], pin: Pin<'_>, dir: &Path, what: &str) -> Checked {
    let mut copy = Ok(());
    let mut notices = Vec::new();
    for (position, source) in sources.iter().enumerate() {
        match download(curl, source, pin, &dir.join(format!("{position}.pinned"))) {
            Ok(()) => println!("{}: gives the pinned {what}", source.name),
            Err(error) if position == 0 => copy = Err(format!("{error} ({})", source.url)),
            Err(error) => notices.push(format!(
                "{} does not give the pinned {what}: {error} ({})",
                source.name, source.url
            )),
        }
    }
    Checked { copy, notices }
}

/// The newest version a listing (a release, an index) names, as `read` finds it in the listing's bytes.
pub fn newest(
    curl: &Curl,
    listing: &Source,
    dir: &Path,
    read: impl FnOnce(&[u8]) -> std::result::Result<String, String>,
) -> std::result::Result<String, String> {
    let file = dir.join("listing");
    get(curl, listing, MAX_LISTING_BYTES, curl.listing, &file)
        .map_err(|e| e.to_string())
        .and_then(|()| fs::read(&file).map_err(|e| e.to_string()))
        .and_then(|bytes| read(&bytes))
}

/// Ends a check: prints its notices, and fails when our copy did not give the pin. `fix` says where to act.
pub fn conclude(checked: Checked, what: &str, fix: &str) -> Result<()> {
    for text in &checked.notices {
        notice(text);
    }
    checked.copy.map_err(|why| {
        format!("our copy of the pinned {what} does not answer: {why}. Publish it ({fix})").into()
    })
}

/// Ends checks that each ran to the end: fails with every failure, one per line, when any failed.
pub fn all(results: impl IntoIterator<Item = Result<()>>) -> Result<()> {
    let failed: Vec<String> = results
        .into_iter()
        .filter_map(|result| result.err())
        .map(|error| error.to_string())
        .collect();
    match failed.is_empty() {
        true => Ok(()),
        false => Err(failed.join("\n").into()),
    }
}

/// A scratch folder for a check, made empty, and removed with what is in it when the check ends.
pub struct Scratch(std::path::PathBuf);

impl Scratch {
    pub fn new(path: std::path::PathBuf) -> Result<Self> {
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
        Ok(Scratch(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A warning: an annotation and a line of the job's summary on GitHub, a line here.
fn notice(text: &str) {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        println!("warning: {text}");
        return;
    }
    println!("::warning::{text}");
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        let line = format!("- {text}\n");
        let _ = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(summary)
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
    }
}

/// Web servers on 127.0.0.1 and curl settings that reach them, for the tests of every pin.
#[cfg(test)]
pub mod testing {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sha2::{Digest, Sha256};

    use super::{Curl, Pin, Source};

    pub const BODY: &[u8] = b"the bytes of a pinned file";
    /// Other bytes of the same length.
    pub const OTHER: &[u8] = b"THE BYTES OF A PINNED FILE";

    pub fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    pub fn pin(hash: &str) -> Pin<'_> {
        Pin {
            size: BODY.len() as u64,
            sha256: hash,
        }
    }

    /// A web server that answers each path with the raw response given for it, 404 otherwise.
    pub fn serve(routes: Vec<(&'static str, Vec<u8>)>) -> u16 {
        serve_logged(routes).0
    }

    /// `serve`, with the paths asked for in order.
    pub fn serve_logged(routes: Vec<(&'static str, Vec<u8>)>) -> (u16, Arc<Mutex<Vec<String>>>) {
        serve_after(routes, Duration::ZERO)
    }

    /// `serve_logged`, each answer given `delay` after its request.
    pub fn serve_after(
        routes: Vec<(&'static str, Vec<u8>)>,
        delay: Duration,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                std::thread::sleep(delay);
                let answer = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map(|(_, raw)| raw.clone())
                    .unwrap_or_else(|| b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec());
                let _ = stream.write_all(&answer);
            }
        });
        (port, asked)
    }

    /// A server that takes connections and never answers.
    pub fn stall() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                held.push(stream);
            }
        });
        port
    }

    pub fn ok(body: &[u8]) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    pub fn redirect(to: &str) -> Vec<u8> {
        format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .into_bytes()
    }

    /// curl over plain HTTP, with `elsewhere.test` resolving to this machine.
    pub fn local(port: u16) -> Curl {
        Curl {
            protocols: "=http",
            extra: vec!["--resolve".into(), format!("elsewhere.test:{port}:127.0.0.1")],
            connect: Duration::from_secs(2),
            file: Duration::from_secs(2),
            listing: Duration::from_secs(2),
        }
    }

    pub fn source(port: u16, path: &str) -> Source {
        Source {
            name: "test",
            url: format!("http://127.0.0.1:{port}{path}"),
            hosts: &["127.0.0.1"],
        }
    }

    pub fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pinned.file");
        (dir, path)
    }

    pub fn nothing_left(dest: &Path) {
        assert!(!dest.exists(), "a refused download left {}", dest.display());
        assert!(
            !dest.with_extension("partial").exists(),
            "a refused download left its partial file"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use super::testing::*;
    use super::*;

    mod deadlines {
        use std::time::Duration;

        use proptest::prelude::*;

        use super::super::curl_seconds;

        /// What curl reads from the argument: seconds as a decimal number, 0 meaning no limit.
        fn parsed(argument: &str) -> Duration {
            Duration::from_secs_f64(argument.parse::<f64>().unwrap())
        }

        #[test]
        fn what_rounds_to_zero_is_no_request() {
            assert_eq!(curl_seconds(Duration::ZERO), None);
            assert_eq!(curl_seconds(Duration::from_nanos(1)), None);
            assert_eq!(curl_seconds(Duration::from_micros(499)), None);
            assert_eq!(curl_seconds(Duration::from_micros(500)), None);
            assert_eq!(curl_seconds(Duration::from_micros(999)), None);
        }

        #[test]
        fn a_millisecond_and_more_is_rounded_down() {
            assert_eq!(curl_seconds(Duration::from_millis(1)).as_deref(), Some("0.001"));
            assert_eq!(
                curl_seconds(Duration::from_micros(1_999)).as_deref(),
                Some("0.001")
            );
            assert_eq!(
                curl_seconds(Duration::from_micros(9_999_999)).as_deref(),
                Some("9.999")
            );
            assert_eq!(curl_seconds(Duration::from_secs(300)).as_deref(), Some("300.000"));
        }

        proptest! {
            #[test]
            fn either_no_request_or_a_limit_within_what_is_left(nanos in 0u64..10_000_000_000_000) {
                let left = Duration::from_nanos(nanos);
                if let Some(argument) = curl_seconds(left) {
                    let limit = parsed(&argument);
                    prop_assert!(limit > Duration::ZERO, "{argument} reads as no limit");
                    prop_assert!(limit <= left, "{argument} is more than {left:?}");
                } else {
                    prop_assert!(left < Duration::from_millis(1), "{left:?} made no request");
                }
            }
        }
    }

    #[test]
    fn the_pinned_bytes_are_kept() {
        let port = serve(vec![("/good", ok(BODY))]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        download(&local(port), &source(port, "/good"), pin(&hash), &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), BODY);
    }

    #[test]
    fn a_404_is_refused() {
        let port = serve(vec![]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&local(port), &source(port, "/gone"), pin(&hash), &dest).unwrap_err();
        assert!(
            matches!(&error, FetchError::Transport(why) if why.contains("404")),
            "{error:?}"
        );
        nothing_left(&dest);
    }

    #[test]
    fn other_bytes_of_the_same_size_are_refused_for_their_hash() {
        assert_eq!(OTHER.len(), BODY.len());
        let port = serve(vec![("/swapped", ok(OTHER))]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&local(port), &source(port, "/swapped"), pin(&hash), &dest).unwrap_err();
        assert_eq!(error, FetchError::Hash(sha(OTHER)));
        nothing_left(&dest);
    }

    #[test]
    fn a_body_cut_off_in_transfer_is_refused() {
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            BODY.len()
        )
        .into_bytes();
        raw.extend_from_slice(&BODY[..10]);
        let port = serve(vec![("/cut", raw)]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&local(port), &source(port, "/cut"), pin(&hash), &dest).unwrap_err();
        assert!(matches!(error, FetchError::Transport(_)), "{error:?}");
        nothing_left(&dest);
    }

    #[test]
    fn a_complete_but_shorter_file_is_refused_for_its_size() {
        let port = serve(vec![("/short", ok(&BODY[..10]))]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&local(port), &source(port, "/short"), pin(&hash), &dest).unwrap_err();
        assert_eq!(error, FetchError::Size(10));
        nothing_left(&dest);
    }

    #[test]
    fn a_longer_file_is_refused() {
        let mut long = BODY.to_vec();
        long.extend_from_slice(&[0; 4096]);
        let port = serve(vec![("/long", ok(&long))]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        // curl stops at one byte past the pin, before the rest of a long body fills the disk.
        let error = download(&local(port), &source(port, "/long"), pin(&hash), &dest).unwrap_err();
        assert!(
            matches!(&error, FetchError::Transport(why) if why.contains("63")),
            "{error:?}"
        );
        nothing_left(&dest);
    }

    #[test]
    fn a_redirect_to_another_host_is_refused_even_with_the_right_bytes() {
        let target = serve(vec![("/good", ok(BODY))]);
        let first = serve(vec![(
            "/moved",
            redirect(&format!("http://elsewhere.test:{target}/good")),
        )]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&local(target), &source(first, "/moved"), pin(&hash), &dest).unwrap_err();
        assert_eq!(error, FetchError::Host("elsewhere.test".into()));
        nothing_left(&dest);
    }

    #[test]
    fn no_request_goes_to_an_unlisted_host_on_the_way_back_to_a_listed_one() {
        let good = serve(vec![("/good", ok(BODY))]);
        let (detour, asked_detour) = serve_logged(vec![(
            "/back",
            redirect(&format!("http://127.0.0.1:{good}/good")),
        )]);
        let first = serve(vec![(
            "/moved",
            redirect(&format!("http://elsewhere.test:{detour}/back")),
        )]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&local(detour), &source(first, "/moved"), pin(&hash), &dest).unwrap_err();
        assert_eq!(error, FetchError::Host("elsewhere.test".into()));
        assert!(
            asked_detour.lock().unwrap().is_empty(),
            "the unlisted host was asked {:?}",
            asked_detour.lock().unwrap()
        );
        nothing_left(&dest);
    }

    #[test]
    fn a_redirect_within_the_named_hosts_is_followed() {
        let target = serve(vec![("/good", ok(BODY))]);
        let first = serve(vec![(
            "/moved",
            redirect(&format!("http://127.0.0.1:{target}/good")),
        )]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        download(&local(first), &source(first, "/moved"), pin(&hash), &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), BODY);
    }

    #[test]
    fn plain_http_is_refused_by_the_real_settings() {
        let port = serve(vec![("/good", ok(BODY))]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&Curl::https(), &source(port, "/good"), pin(&hash), &dest).unwrap_err();
        assert!(matches!(error, FetchError::Transport(_)), "{error:?}");
        nothing_left(&dest);
    }

    #[test]
    fn a_build_falls_back_to_the_next_source_and_fails_when_none_gives_the_pin() {
        let port = serve(vec![("/good", ok(BODY)), ("/wrong", ok(OTHER))]);
        let hash = sha(BODY);
        let (_dir, dest) = scratch();
        fetch(
            &local(port),
            &[source(port, "/gone"), source(port, "/good")],
            pin(&hash),
            &dest,
            "file",
        )
        .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), BODY);

        let (_dir, dest) = scratch();
        let error = fetch(
            &local(port),
            &[source(port, "/gone"), source(port, "/wrong")],
            pin(&hash),
            &dest,
            "monitor",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("no source gave the pinned monitor")
                && error.contains("404")
                && error.contains("SHA-256"),
            "{error}"
        );
        nothing_left(&dest);
    }

    #[test]
    fn our_copy_is_asked_first_and_alone_when_it_answers() {
        let (port, asked) = serve_logged(vec![("/copy", ok(BODY)), ("/project", ok(BODY))]);
        let hash = sha(BODY);
        let (_dir, dest) = scratch();
        fetch(
            &local(port),
            &[source(port, "/copy"), source(port, "/project")],
            pin(&hash),
            &dest,
            "file",
        )
        .unwrap();
        assert_eq!(*asked.lock().unwrap(), ["/copy"]);
    }

    #[test]
    fn a_file_that_holds_the_pin_is_kept_without_a_request_and_any_other_is_fetched_anew() {
        let (port, asked) = serve_logged(vec![("/copy", ok(BODY))]);
        let hash = sha(BODY);
        let dir = tempfile::tempdir().unwrap();
        // A folder that is not there yet is made.
        let dest = dir.path().join("programs/pinned.file");
        let sources = [source(port, "/copy")];
        ensure(&local(port), &sources, pin(&hash), &dest, "file").unwrap();
        ensure(&local(port), &sources, pin(&hash), &dest, "file").unwrap();
        assert_eq!(asked.lock().unwrap().len(), 1, "the second call asked again");
        for stale in [OTHER, &BODY[..10], b""] {
            fs::write(&dest, stale).unwrap();
            ensure(&local(port), &sources, pin(&hash), &dest, "file").unwrap();
            assert_eq!(fs::read(&dest).unwrap(), BODY);
        }
        assert_eq!(asked.lock().unwrap().len(), 4);
    }

    #[test]
    fn a_stale_file_stays_out_of_the_build_when_no_source_gives_the_pin() {
        let port = serve(vec![("/wrong", ok(OTHER))]);
        let hash = sha(BODY);
        let (_dir, dest) = scratch();
        fs::write(&dest, OTHER).unwrap();
        ensure(&local(port), &[source(port, "/wrong")], pin(&hash), &dest, "file").unwrap_err();
        assert_eq!(
            fs::read(&dest).unwrap(),
            OTHER,
            "nothing but a whole pinned file replaces it"
        );
        assert!(!dest.with_extension("partial").exists());
    }

    #[test]
    fn a_redirect_loop_ends() {
        let (looping, asked) = serve_logged(vec![("/again", redirect("/again"))]);
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&local(looping), &source(looping, "/again"), pin(&hash), &dest).unwrap_err();
        assert_eq!(error, FetchError::Transport("more than 5 redirects".into()));
        assert_eq!(
            asked.lock().unwrap().len(),
            6,
            "the first request and five redirects"
        );
        nothing_left(&dest);
    }

    #[test]
    fn a_deadline_below_a_millisecond_asks_nothing() {
        let (port, asked) = serve_logged(vec![("/good", ok(BODY))]);
        let curl = Curl {
            file: Duration::from_micros(500),
            ..local(port)
        };
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let error = download(&curl, &source(port, "/good"), pin(&hash), &dest).unwrap_err();
        assert_eq!(error, FetchError::Transport("no answer within 0 s".into()));
        assert!(
            asked.lock().unwrap().is_empty(),
            "asked {:?}",
            asked.lock().unwrap()
        );
        nothing_left(&dest);
    }

    #[test]
    fn the_deadline_covers_every_hop_together() {
        // Each hop answers in 0.7 s, well inside the 2 s deadline; four of them are not.
        let (port, _) = serve_after(
            vec![
                ("/1", redirect("/2")),
                ("/2", redirect("/3")),
                ("/3", redirect("/4")),
                ("/4", ok(BODY)),
            ],
            Duration::from_millis(700),
        );
        let (_dir, dest) = scratch();
        let hash = sha(BODY);
        let started = Instant::now();
        let error = download(&local(port), &source(port, "/1"), pin(&hash), &dest).unwrap_err();
        assert!(matches!(error, FetchError::Transport(_)), "{error:?}");
        assert!(
            started.elapsed() < Duration::from_millis(2600),
            "took {:?}",
            started.elapsed()
        );
        nothing_left(&dest);
    }

    #[test]
    fn only_our_copy_failing_fails_a_check() {
        let port = serve(vec![("/copy", ok(BODY)), ("/wrong", ok(OTHER))]);
        let stalled = stall();
        let hash = sha(BODY);
        let dir = tempfile::tempdir().unwrap();
        let checked = check_sources(
            &local(port),
            &[
                source(port, "/copy"),
                source(port, "/gone"),
                source(stalled, "/x"),
            ],
            pin(&hash),
            dir.path(),
            "monitor",
        );
        assert_eq!(checked.copy, Ok(()));
        assert_eq!(checked.notices.len(), 2, "{:?}", checked.notices);
        assert!(
            checked.notices[0].starts_with("test does not give the pinned monitor: the answer was HTTP 404"),
            "{:?}",
            checked.notices
        );
        assert!(conclude(checked, "monitor", "here").is_ok());

        for (copy, why) in [
            (source(port, "/gone"), "HTTP 404"),
            (source(port, "/wrong"), "SHA-256"),
            (source(stalled, "/copy"), "curl failed"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let started = Instant::now();
            let checked = check_sources(
                &local(port),
                &[copy, source(port, "/copy")],
                pin(&hash),
                dir.path(),
                "monitor",
            );
            let error = checked.copy.as_ref().unwrap_err().clone();
            assert!(error.contains(why), "{error}");
            assert!(checked.notices.is_empty(), "{:?}", checked.notices);
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "took {:?}",
                started.elapsed()
            );
            let error = conclude(checked, "monitor", "here").unwrap_err().to_string();
            assert!(
                error.starts_with("our copy of the pinned monitor does not answer")
                    && error.ends_with("Publish it (here)"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_scratch_folder_starts_empty_and_goes_with_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("check");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("left from before"), b"x").unwrap();
        {
            let scratch = Scratch::new(path.clone()).unwrap();
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
            fs::write(scratch.path().join("0.pinned"), b"x").unwrap();
        }
        assert!(!path.exists());
    }

    #[test]
    fn hosts_are_read_without_user_port_or_path() {
        assert_eq!(host_of("https://GitHub.com/a/b").as_deref(), Some("github.com"));
        assert_eq!(
            host_of("https://user:pw@evil.test:443/x").as_deref(),
            Some("evil.test")
        );
        assert_eq!(
            host_of("https://github.com@evil.test/x").as_deref(),
            Some("evil.test")
        );
        assert_eq!(host_of("http://[::1]:80/").as_deref(), Some("::1"));
        assert_eq!(host_of("https://h.test?x=/y").as_deref(), Some("h.test"));
        assert_eq!(host_of("no scheme"), None);
        assert_eq!(host_of("https:///path"), None);
    }
}
