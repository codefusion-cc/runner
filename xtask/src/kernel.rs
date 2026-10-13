//! The guest kernel: Alpine's `linux-virt` for aarch64, pinned by its version, size and SHA-256.
//!
//! Alpine ships the kernel as an EFI "zboot" image holding a gzip-compressed arm64 `Image`.
//! Virtualization.framework boots only the plain `Image`, so it is unpacked here.
//!
//! Alpine keeps only the newest build of a package in a release's `main/`: the URL of a pinned package answers 404
//! the day the next patch comes out, and no official archive keeps the old one. So the build reads the pinned
//! package from our own copy first, a release of this repository tagged `kernel-linux-virt-<version>`, and from
//! Alpine only when the copy is missing. Whichever answers, the bytes must have the pinned size and SHA-256: HTTPS
//! only carries them. Nothing in a build asks Alpine what is newest. `cargo xtask kernel check`, run weekly by
//! `.github/workflows/kernel.yml`, goes red when our copy stops answering with the pinned bytes, and says when
//! Alpine has a newer build.
//!
//! The pinned package was checked against Alpine's own signature (2026-10-13, key
//! `alpine-devel@lists.alpinelinux.org-616ae350.rsa.pub`, the same file from alpinelinux.org/keys and from the
//! aports repository). To move the pin, check the new package the same way, publish our copy of it (below), then
//! change the three numbers here. An apk is three gzip streams: a tar with the signature, a tar with `.PKGINFO`,
//! and the files.
//!
//! 1. `openssl dgst -sha1 -verify <key> -signature <the .SIGN.RSA file of stream 1> <stream 2 as it is>` says
//!    `Verified OK`.
//! 2. `datahash` in `.PKGINFO` is the SHA-256 of stream 3 as it is.
//! 3. `pkgname`, `pkgver` and `arch` in `.PKGINFO` are the ones wanted.
//!
//! Our copy: `gh release create kernel-linux-virt-<version> <the checked .apk> --prerelease --latest=false`, by
//! the owner. The tag does not start with `image-`, so nothing takes it for a guest image.

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::Result;

/// The package's version, part of the bundle's version.
pub const APK_VERSION: &str = "6.18.56-r0";
/// SHA-256 of the package, which pins the kernel and every module.
pub const APK_SHA256: &str = "e3839bfd751ff0a4421484f712b9b29efb514024523736a0ef62521521571b12";
/// The package's size in bytes. A download that is longer is cut off by curl, one that is shorter is refused.
pub const APK_SIZE: u64 = 41_695_630;

/// The Alpine release whose `main/` the package comes from.
const ALPINE_BRANCH: &str = "v3.24";

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

/// The pinned package.
pub const PIN: Pin<'static> = Pin {
    size: APK_SIZE,
    sha256: APK_SHA256,
};

/// A place the package can be read from, and the hosts its answer may come from after redirects.
#[derive(Clone, Debug)]
pub struct Source {
    pub name: &'static str,
    pub url: String,
    pub hosts: &'static [&'static str],
}

/// Where a build reads the pinned package, in order: our copy, then Alpine.
pub fn sources() -> [Source; 2] {
    [
        Source {
            name: "our copy",
            url: format!(
                "https://github.com/codefusion-cc/runner/releases/download/kernel-linux-virt-{APK_VERSION}/linux-virt-{APK_VERSION}.apk"
            ),
            // A release asset answers with a redirect to GitHub's storage.
            hosts: &[
                "github.com",
                "release-assets.githubusercontent.com",
                "objects.githubusercontent.com",
            ],
        },
        Source {
            name: "Alpine",
            url: format!(
                "https://dl-cdn.alpinelinux.org/alpine/{ALPINE_BRANCH}/main/aarch64/linux-virt-{APK_VERSION}.apk"
            ),
            hosts: &["dl-cdn.alpinelinux.org"],
        },
    ]
}

/// How a download goes over the network: curl, HTTPS only.
pub struct Curl {
    /// curl's `--proto` and `--proto-redir`.
    protocols: &'static str,
    /// More arguments, for tests (`--resolve`).
    extra: Vec<String>,
}

impl Curl {
    pub fn https() -> Self {
        Curl {
            protocols: "=https",
            extra: Vec::new(),
        }
    }
}

/// Why a source did not give the pinned bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// curl failed: no answer, an HTTP error status (404 among them), a body cut off, too long, too many redirects.
    Transport(String),
    /// The answer came from a host the source does not name.
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
            FetchError::Host(host) => {
                write!(f, "the answer came from {host}, which the source does not name")
            }
            FetchError::Size(size) => write!(f, "the package has {size} bytes, not the pinned size"),
            FetchError::Hash(hash) => write!(f, "the package has SHA-256 {hash}, not the pinned one"),
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
    let _ = fs::remove_file(&partial);
    let result = download_to(curl, source, pin, &partial);
    match result {
        Ok(()) => fs::rename(&partial, dest).map_err(|e| FetchError::Transport(e.to_string())),
        Err(error) => {
            let _ = fs::remove_file(&partial);
            Err(error)
        }
    }
}

fn download_to(
    curl: &Curl,
    source: &Source,
    pin: Pin<'_>,
    partial: &Path,
) -> std::result::Result<(), FetchError> {
    let output = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "--max-redirs", "5"])
        .args(["--proto", curl.protocols, "--proto-redir", curl.protocols])
        // One byte more than the pin, so a longer body is cut off and refused below for its size.
        .args(["--max-filesize", &(pin.size + 1).to_string()])
        .args(["-w", "%{url_effective}", "-o"])
        .arg(partial)
        .args(&curl.extra)
        .arg(&source.url)
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
    let effective = String::from_utf8_lossy(&output.stdout);
    let host = host_of(&effective).unwrap_or_default();
    if !source.hosts.contains(&host.as_str()) {
        return Err(FetchError::Host(host));
    }
    let bytes = fs::read(partial).map_err(|e| FetchError::Transport(e.to_string()))?;
    pin.check(&bytes)
}

/// Tries each source in order and keeps the first that gives the pinned bytes; says what each one before it did.
pub fn fetch(curl: &Curl, sources: &[Source], pin: Pin<'_>, dest: &Path) -> Result<()> {
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
    Err(format!(
        "no source gave the pinned kernel package:\n  {}",
        failures.join("\n  ")
    )
    .into())
}

/// The host of a URL, lower case, without user, port or path.
fn host_of(url: &str) -> Option<String> {
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

/// `cargo xtask kernel check`: every source of the pin, and what Alpine has now. Fails when our copy does not give
/// the pinned bytes, since a build would then live on a URL Alpine deletes with the next patch.
pub fn check(root: &Path, args: &[String]) -> Result<()> {
    crate::no_options(args)?;
    let dir = root.join("target/kernel-check");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let curl = Curl::https();
    let mut copy_ok = true;
    for (index, source) in sources().iter().enumerate() {
        match download(&curl, source, PIN, &dir.join(format!("{index}.apk"))) {
            Ok(()) => println!("{}: gives the pinned package {APK_VERSION}", source.name),
            Err(error) => {
                println!("{}: {error} ({})", source.name, source.url);
                copy_ok &= index != 0;
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
    match alpine_newest() {
        Ok(newest) if newest == APK_VERSION => println!("Alpine {ALPINE_BRANCH} has no newer linux-virt"),
        Ok(newest) => notice(&format!(
            "Alpine {ALPINE_BRANCH} has linux-virt {newest}, the pin is {APK_VERSION}: move the pin (xtask/src/kernel.rs)"
        )),
        Err(error) => notice(&format!("could not read Alpine's index: {error}")),
    }
    if copy_ok {
        Ok(())
    } else {
        Err(
            "our copy of the pinned kernel package is missing or wrong: publish it (xtask/src/kernel.rs)"
                .into(),
        )
    }
}

fn notice(text: &str) {
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        println!("::warning::{text}");
    } else {
        println!("{text}");
    }
}

/// The version of `linux-virt` in the Alpine branch's index now. Read by `check` only, never by a build.
fn alpine_newest() -> Result<String> {
    let url = format!("https://dl-cdn.alpinelinux.org/alpine/{ALPINE_BRANCH}/main/aarch64/APKINDEX.tar.gz");
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "3",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
        ])
        .arg(&url)
        .output()?;
    if !output.status.success() {
        return Err(format!("curl failed with {}", output.status).into());
    }
    newest_in_index(&output.stdout)
}

/// The `linux-virt` version in an `APKINDEX.tar.gz`: gzip streams of one tar whose `APKINDEX` member holds records
/// of `K:value` lines, separated by blank lines.
fn newest_in_index(archive: &[u8]) -> Result<String> {
    let mut tar = Vec::new();
    let mut rest = archive;
    while !rest.is_empty() {
        let mut decoder = flate2::bufread::GzDecoder::new(rest);
        decoder.read_to_end(&mut tar)?;
        rest = decoder.into_inner();
    }
    let index = tar_member(&tar, "APKINDEX").ok_or("the index has no APKINDEX")?;
    let index = String::from_utf8_lossy(index);
    for record in index.split("\n\n") {
        let field = |key: &str| record.lines().find_map(|line| line.strip_prefix(key));
        if field("P:") == Some("linux-virt") {
            return field("V:")
                .map(str::to_string)
                .ok_or_else(|| "linux-virt has no version in the index".into());
        }
    }
    Err("linux-virt is not in the index".into())
}

/// A file's bytes in a ustar archive, by name.
fn tar_member<'a>(tar: &'a [u8], name: &str) -> Option<&'a [u8]> {
    let mut at = 0;
    while at + 512 <= tar.len() {
        let header = &tar[at..at + 512];
        if header.iter().all(|&b| b == 0) {
            at += 512;
            continue;
        }
        let field = |range: std::ops::Range<usize>| {
            let raw = &header[range];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).trim().to_string()
        };
        let size = usize::from_str_radix(&field(124..136), 8).ok()?;
        let start = at + 512;
        let body = tar.get(start..start.checked_add(size)?)?;
        if field(0..100) == name {
            return Some(body);
        }
        at = start + size.div_ceil(512) * 512;
    }
    None
}

/// The arm64 `Image` header carries "ARM\x64" at offset 56.
const ARM64_MAGIC: &[u8] = b"ARM\x64";

/// Unpacks an EFI zboot image (`zimg` header, a payload offset and size, a compression name) to the plain kernel.
pub fn unpack_zboot(image: &[u8]) -> Result<Vec<u8>> {
    if image.len() < 32 || &image[0..2] != b"MZ" || &image[4..8] != b"zimg" {
        return Err("the kernel is not an EFI zboot image".into());
    }
    let offset = u32::from_le_bytes(image[8..12].try_into()?) as usize;
    let size = u32::from_le_bytes(image[12..16].try_into()?) as usize;
    let compression = &image[24..32];
    if !compression.starts_with(b"gzip") {
        return Err(format!(
            "the kernel payload is compressed with {:?}, not gzip",
            String::from_utf8_lossy(compression)
        )
        .into());
    }
    let payload = offset
        .checked_add(size)
        .and_then(|end| image.get(offset..end))
        .ok_or("the zboot payload is cut off")?;
    let mut kernel = Vec::new();
    GzDecoder::new(payload).read_to_end(&mut kernel)?;
    if kernel.get(56..60) != Some(ARM64_MAGIC) {
        return Err("the unpacked kernel is not an arm64 Image".into());
    }
    Ok(kernel)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::GzEncoder};

    use super::*;

    mod fetching {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::path::PathBuf;
        use std::sync::{Arc, Mutex};

        use sha2::{Digest, Sha256};

        use super::super::*;

        const BODY: &[u8] = b"the bytes of a kernel package";

        fn sha(bytes: &[u8]) -> String {
            format!("{:x}", Sha256::digest(bytes))
        }

        fn pin(hash: &str) -> Pin<'_> {
            Pin {
                size: BODY.len() as u64,
                sha256: hash,
            }
        }

        /// A web server on 127.0.0.1 that answers each path with the raw response given for it, 404 otherwise.
        fn serve(routes: Vec<(&'static str, Vec<u8>)>) -> u16 {
            serve_logged(routes).0
        }

        /// `serve`, with the paths asked for in order.
        fn serve_logged(routes: Vec<(&'static str, Vec<u8>)>) -> (u16, Arc<Mutex<Vec<String>>>) {
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

        fn ok(body: &[u8]) -> Vec<u8> {
            let mut raw = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            raw.extend_from_slice(body);
            raw
        }

        fn redirect(to: &str) -> Vec<u8> {
            format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .into_bytes()
        }

        /// curl over plain HTTP, with `elsewhere.test` resolving to this machine.
        fn local(port: u16) -> Curl {
            Curl {
                protocols: "=http",
                extra: vec!["--resolve".into(), format!("elsewhere.test:{port}:127.0.0.1")],
            }
        }

        fn source(port: u16, path: &str) -> Source {
            Source {
                name: "test",
                url: format!("http://127.0.0.1:{port}{path}"),
                hosts: &["127.0.0.1"],
            }
        }

        fn scratch() -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("package.apk");
            (dir, path)
        }

        fn nothing_left(dest: &Path) {
            assert!(!dest.exists(), "a refused download left {}", dest.display());
            assert!(
                !dest.with_extension("partial").exists(),
                "a refused download left its partial file"
            );
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
            let other = b"THE BYTES OF A KERNEL PACKAGE";
            assert_eq!(other.len(), BODY.len());
            let port = serve(vec![("/swapped", ok(other))]);
            let (_dir, dest) = scratch();
            let hash = sha(BODY);
            let error = download(&local(port), &source(port, "/swapped"), pin(&hash), &dest).unwrap_err();
            assert_eq!(error, FetchError::Hash(sha(other)));
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
            let port = serve(vec![
                ("/good", ok(BODY)),
                ("/wrong", ok(b"THE BYTES OF A KERNEL PACKAGE")),
            ]);
            let hash = sha(BODY);
            let (_dir, dest) = scratch();
            fetch(
                &local(port),
                &[source(port, "/gone"), source(port, "/good")],
                pin(&hash),
                &dest,
            )
            .unwrap();
            assert_eq!(fs::read(&dest).unwrap(), BODY);

            let (_dir, dest) = scratch();
            let error = fetch(
                &local(port),
                &[source(port, "/gone"), source(port, "/wrong")],
                pin(&hash),
                &dest,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("404") && error.contains("SHA-256"), "{error}");
            nothing_left(&dest);
        }

        #[test]
        fn our_copy_is_asked_first_and_alone_when_it_answers() {
            let (port, asked) = serve_logged(vec![("/copy", ok(BODY)), ("/alpine", ok(BODY))]);
            let hash = sha(BODY);
            let (_dir, dest) = scratch();
            fetch(
                &local(port),
                &[source(port, "/copy"), source(port, "/alpine")],
                pin(&hash),
                &dest,
            )
            .unwrap();
            assert_eq!(*asked.lock().unwrap(), ["/copy"]);
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

        #[test]
        fn the_sources_are_https_and_name_the_pinned_version() {
            for source in sources() {
                assert!(source.url.starts_with("https://"), "{}", source.url);
                assert!(
                    source.url.ends_with(&format!("linux-virt-{APK_VERSION}.apk")),
                    "{}",
                    source.url
                );
                assert!(source.hosts.contains(&host_of(&source.url).unwrap().as_str()));
            }
            assert_eq!(APK_SHA256.len(), 64);
        }

        /// An index as Alpine signs it: a gzip stream with the signature, then one with the index, from real tar.
        fn index_archive(records: &str) -> Vec<u8> {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(".SIGN.RSA.key.pub"), b"signature").unwrap();
            fs::write(dir.path().join("DESCRIPTION"), b"v3.24").unwrap();
            fs::write(dir.path().join("APKINDEX"), records).unwrap();
            let tar = |names: &[&str]| {
                let out = std::process::Command::new("tar")
                    .arg("-cf")
                    .arg("-")
                    .arg("-C")
                    .arg(dir.path())
                    .args(names)
                    .output()
                    .unwrap();
                assert!(out.status.success());
                // bsdtar pads what it compresses itself to a whole record, outside the gzip stream.
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                gz.write_all(&out.stdout).unwrap();
                gz.finish().unwrap()
            };
            let mut archive = tar(&[".SIGN.RSA.key.pub"]);
            archive.extend(tar(&["DESCRIPTION", "APKINDEX"]));
            archive
        }

        #[test]
        fn the_index_names_the_version_of_linux_virt() {
            let records = "C:Q1a=\nP:linux-lts\nV:6.12.1-r0\n\nC:Q1b=\nP:linux-virt\nV:6.18.57-r0\nA:aarch64\n\nP:musl\nV:1.2.5-r9\n";
            assert_eq!(newest_in_index(&index_archive(records)).unwrap(), "6.18.57-r0");
        }

        #[test]
        fn an_index_without_linux_virt_or_damaged_is_an_error() {
            assert!(newest_in_index(&index_archive("P:linux-virt-dev\nV:1-r0\n\nP:musl\nV:1\n")).is_err());
            assert!(newest_in_index(&index_archive("P:linux-virt\nA:aarch64\n")).is_err());
            assert!(newest_in_index(b"not gzip").is_err());
            let mut cut = index_archive("P:linux-virt\nV:6.18.57-r0\n");
            cut.truncate(cut.len() / 2);
            assert!(newest_in_index(&cut).is_err());
        }
    }

    fn zboot(payload_kernel: &[u8], compression: &[u8; 8]) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(payload_kernel).unwrap();
        let payload = gz.finish().unwrap();
        let mut image = vec![0u8; 64];
        image[0..2].copy_from_slice(b"MZ");
        image[4..8].copy_from_slice(b"zimg");
        image[8..12].copy_from_slice(&64u32.to_le_bytes());
        image[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        image[24..32].copy_from_slice(compression);
        image.extend(payload);
        image
    }

    fn kernel() -> Vec<u8> {
        let mut kernel = vec![0u8; 128];
        kernel[56..60].copy_from_slice(ARM64_MAGIC);
        kernel
    }

    #[test]
    fn a_zboot_image_unpacks_to_the_kernel() {
        assert_eq!(
            unpack_zboot(&zboot(&kernel(), b"gzip\0\0\0\0")).unwrap(),
            kernel()
        );
    }

    #[test]
    fn what_is_not_a_gzip_zboot_arm64_image_is_refused() {
        assert!(unpack_zboot(b"short").is_err());
        assert!(unpack_zboot(&zboot(&kernel(), b"zstd22\0\0")).is_err());
        assert!(unpack_zboot(&zboot(&[0u8; 128], b"gzip\0\0\0\0")).is_err());
        let mut cut = zboot(&kernel(), b"gzip\0\0\0\0");
        cut.truncate(70);
        assert!(unpack_zboot(&cut).is_err());
        let mut lying = zboot(&kernel(), b"gzip\0\0\0\0");
        lying[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(unpack_zboot(&lying).is_err());
    }
}
