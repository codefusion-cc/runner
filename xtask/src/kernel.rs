//! The guest kernel: Alpine's `linux-virt` for aarch64, pinned by its version, size and SHA-256.
//!
//! Alpine ships the kernel as an EFI "zboot" image holding a gzip-compressed arm64 `Image`.
//! Virtualization.framework boots only the plain `Image`, so it is unpacked here.
//!
//! Alpine keeps only the newest build of a package in a release's `main/`: the URL of a pinned package answers 404
//! the day the next patch comes out, and no official archive keeps the old one. So the build reads the pinned
//! package from our own copy first, a release of this repository tagged `kernel-linux-virt-<version>`, and from
//! Alpine only when the copy is missing. Whichever answers, the bytes must have the pinned size and SHA-256: HTTPS
//! only carries them (`pinned.rs`). Nothing in a build asks Alpine what is newest. `cargo xtask kernel check`, run weekly by
//! `.github/workflows/pins.yml`, goes red when our copy stops answering with the pinned bytes, and says when
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

use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;

use crate::Result;
use crate::pinned::{self, Checked, Curl, GITHUB_RELEASE_HOSTS, Pin, Scratch, Source};

/// The package's version, part of the bundle's version.
pub const APK_VERSION: &str = "6.18.56-r0";
/// SHA-256 of the package, which pins the kernel and every module.
pub const APK_SHA256: &str = "e3839bfd751ff0a4421484f712b9b29efb514024523736a0ef62521521571b12";
/// The package's size in bytes. A download that is longer is cut off by curl, one that is shorter is refused.
pub const APK_SIZE: u64 = 41_695_630;

/// The Alpine release whose `main/` the package comes from.
const ALPINE_BRANCH: &str = "v3.24";

/// The pinned package.
pub const PIN: Pin<'static> = Pin {
    size: APK_SIZE,
    sha256: APK_SHA256,
};

/// Where a build reads the pinned package, in order: our copy, then Alpine.
pub fn sources() -> [Source; 2] {
    [
        Source {
            name: "our copy",
            url: format!(
                "https://github.com/codefusion-cc/runner/releases/download/kernel-linux-virt-{APK_VERSION}/linux-virt-{APK_VERSION}.apk"
            ),
            hosts: GITHUB_RELEASE_HOSTS,
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

/// Alpine's index of the branch the package comes from.
fn alpine_index() -> Source {
    Source {
        name: "Alpine's index",
        url: format!("https://dl-cdn.alpinelinux.org/alpine/{ALPINE_BRANCH}/main/aarch64/APKINDEX.tar.gz"),
        hosts: &["dl-cdn.alpinelinux.org"],
    }
}

/// What the build calls the package.
pub const WHAT: &str = "kernel package";

/// `cargo xtask kernel check`: fails only when our copy does not give the pinned bytes, since a build would then
/// live on a URL Alpine deletes with the next patch. Alpine slow, down or newer is a warning.
pub fn check(root: &Path, args: &[String]) -> Result<()> {
    crate::no_options(args)?;
    let dir = Scratch::new(root.join("target/kernel-check"))?;
    let checked = check_sources(&Curl::https(), &sources(), PIN, &alpine_index(), dir.path());
    pinned::conclude(checked, WHAT, "xtask/src/kernel.rs")
}

fn check_sources(curl: &Curl, sources: &[Source], pin: Pin<'_>, index: &Source, dir: &Path) -> Checked {
    let mut checked = pinned::check_sources(curl, sources, pin, dir, WHAT);
    match pinned::newest(curl, index, dir, |bytes| newest_in_index(bytes).map_err(|e| e.to_string())) {
        Ok(newest) if newest == APK_VERSION => println!("Alpine {ALPINE_BRANCH} has no newer linux-virt"),
        Ok(newest) => checked.notices.push(format!(
            "Alpine {ALPINE_BRANCH} has linux-virt {newest}, the pin is {APK_VERSION}: move the pin (xtask/src/kernel.rs)"
        )),
        Err(error) => checked.notices.push(format!("could not read Alpine's index: {error}")),
    }
    checked
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

    mod checking {
        use std::io::Write;
        use std::time::{Duration, Instant};

        use super::super::*;
        use crate::pinned::host_of;
        use crate::pinned::testing::*;

        fn index_source(port: u16, path: &str) -> Source {
            Source {
                name: "Alpine's index",
                ..source(port, path)
            }
        }

        fn current_index() -> Vec<u8> {
            index_archive(&format!("P:linux-virt\nV:{APK_VERSION}\n"))
        }

        #[test]
        fn alpine_stalling_is_a_warning_and_waits_no_longer_than_the_deadline() {
            let good = serve(vec![("/copy", ok(BODY))]);
            let stalled = stall();
            let dir = tempfile::tempdir().unwrap();
            let hash = sha(BODY);
            let started = Instant::now();
            let checked = check_sources(
                &local(good),
                &[source(good, "/copy"), source(stalled, "/alpine")],
                pin(&hash),
                &index_source(stalled, "/APKINDEX.tar.gz"),
                dir.path(),
            );
            assert_eq!(checked.copy, Ok(()));
            assert_eq!(checked.notices.len(), 2, "{:?}", checked.notices);
            assert!(
                checked.notices[0].starts_with("test does not give the pinned kernel package"),
                "{:?}",
                checked.notices
            );
            assert!(
                checked.notices[1].starts_with("could not read Alpine's index"),
                "{:?}",
                checked.notices
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "took {:?}",
                started.elapsed()
            );
        }

        #[test]
        fn alpine_gone_or_newer_is_a_warning() {
            let port = serve(vec![
                ("/copy", ok(BODY)),
                (
                    "/APKINDEX.tar.gz",
                    ok(&index_archive("P:linux-virt\nV:9.9.9-r0\n")),
                ),
            ]);
            let dir = tempfile::tempdir().unwrap();
            let hash = sha(BODY);
            let checked = check_sources(
                &local(port),
                &[source(port, "/copy"), source(port, "/gone")],
                pin(&hash),
                &index_source(port, "/APKINDEX.tar.gz"),
                dir.path(),
            );
            assert_eq!(checked.copy, Ok(()));
            assert!(checked.notices[0].contains("HTTP 404"), "{:?}", checked.notices);
            assert!(
                checked.notices[1].contains("has linux-virt 9.9.9-r0"),
                "{:?}",
                checked.notices
            );
            assert_eq!(checked.notices.len(), 2);
        }

        #[test]
        fn everything_current_says_nothing() {
            let port = serve(vec![
                ("/copy", ok(BODY)),
                ("/alpine", ok(BODY)),
                ("/APKINDEX.tar.gz", ok(&current_index())),
            ]);
            let dir = tempfile::tempdir().unwrap();
            let hash = sha(BODY);
            let checked = check_sources(
                &local(port),
                &[source(port, "/copy"), source(port, "/alpine")],
                pin(&hash),
                &index_source(port, "/APKINDEX.tar.gz"),
                dir.path(),
            );
            assert_eq!(checked.copy, Ok(()));
            assert!(checked.notices.is_empty(), "{:?}", checked.notices);
        }

        #[test]
        fn our_copy_missing_wrong_or_stalled_is_red_even_when_alpine_answers() {
            let port = serve(vec![
                ("/wrong", ok(OTHER)),
                ("/alpine", ok(BODY)),
                ("/APKINDEX.tar.gz", ok(&current_index())),
            ]);
            let stalled = stall();
            let hash = sha(BODY);
            for (copy, why) in [
                (source(port, "/gone"), "HTTP 404"),
                (source(port, "/wrong"), "SHA-256"),
                (source(stalled, "/copy"), "curl failed"),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let started = Instant::now();
                let checked = check_sources(
                    &local(port),
                    &[copy, source(port, "/alpine")],
                    pin(&hash),
                    &index_source(port, "/APKINDEX.tar.gz"),
                    dir.path(),
                );
                let error = checked.copy.unwrap_err();
                assert!(error.contains(why), "{error}");
                assert!(checked.notices.is_empty(), "{:?}", checked.notices);
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "took {:?}",
                    started.elapsed()
                );
            }
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
            std::fs::write(dir.path().join(".SIGN.RSA.key.pub"), b"signature").unwrap();
            std::fs::write(dir.path().join("DESCRIPTION"), b"v3.24").unwrap();
            std::fs::write(dir.path().join("APKINDEX"), records).unwrap();
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
