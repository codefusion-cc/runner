//! The guest kernel: Alpine's `linux-virt`, one package for each architecture ([`Arch`]), each pinned by its
//! version, size and SHA-256.
//!
//! Alpine ships the arm64 kernel as an EFI "zboot" image holding a gzip-compressed `Image`, and the x86_64 one as a
//! bzImage holding a gzip-compressed `vmlinux`. Virtualization.framework and cloud-hypervisor boot only the plain
//! `Image` on arm64, and cloud-hypervisor boots x86_64 only through PVH, from the `vmlinux` ELF with its PVH entry
//! note (`CONFIG_PVH`), so both are unpacked here and anything else is refused.
//!
//! Alpine keeps only the newest build of a package in a release's `main/`: the URL of a pinned package answers 404
//! the day the next patch comes out, and no official archive keeps the old one. So the build reads the pinned
//! package from our own copy first, a release of this repository tagged `kernel-linux-virt-<version>` (arm64) or
//! `kernel-linux-virt-x86_64-<version>`, and from Alpine only when the copy is missing. Whichever answers, the bytes
//! must have the pinned size and SHA-256: HTTPS only carries them (`pinned.rs`). Nothing in a build asks Alpine what
//! is newest. `cargo xtask kernel check`, run weekly by `.github/workflows/pins.yml`, goes red when our copy of
//! either stops answering with the pinned bytes, and says when Alpine has a newer build.
//!
//! Each pinned package was checked against Alpine's own signature: arm64 on 2026-10-13 with key
//! `alpine-devel@lists.alpinelinux.org-616ae350.rsa.pub`, x86_64 on 2026-10-27 with key
//! `alpine-devel@lists.alpinelinux.org-6165ee59.rsa.pub`, each the same file from alpinelinux.org/keys and from the
//! aports repository. To move a pin, check the new package the same way, publish our copy of it (below), then
//! change its three numbers here. An apk is three gzip streams: a tar with the signature, a tar with `.PKGINFO`, and
//! the files.
//!
//! 1. `openssl dgst -sha1 -verify <key> -signature <the .SIGN.RSA file of stream 1> <stream 2 as it is>` says
//!    `Verified OK`.
//! 2. `datahash` in `.PKGINFO` is the SHA-256 of stream 3 as it is.
//! 3. `pkgname`, `pkgver` and `arch` in `.PKGINFO` are the ones wanted.
//!
//! Our copy: `gh release create <tag> <the checked .apk> --prerelease --latest=false`, by the owner, with the tag
//! [`Kernel::tag`] names. The tag does not start with `image-`, so nothing takes it for a guest image.

use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;

use crate::Result;
use crate::arch::Arch;
use crate::pinned::{self, Checked, Curl, GITHUB_RELEASE_HOSTS, Pin, Scratch, Source};

/// The Alpine release whose `main/` the packages come from.
const ALPINE_BRANCH: &str = "v3.24";

/// One architecture's pinned kernel package.
#[derive(Debug)]
pub struct Kernel {
    pub arch: Arch,
    /// The package's version, part of the bundle's version.
    pub version: &'static str,
    /// SHA-256 of the package, which pins the kernel and every module.
    pub sha256: &'static str,
    /// The package's size in bytes. A download that is longer is cut off by curl, one that is shorter is refused.
    pub size: u64,
}

pub const AARCH64: Kernel = Kernel {
    arch: Arch::Aarch64,
    version: "6.18.56-r0",
    sha256: "e3839bfd751ff0a4421484f712b9b29efb514024523736a0ef62521521571b12",
    size: 41_695_630,
};

pub const X86_64: Kernel = Kernel {
    arch: Arch::X86_64,
    version: "6.18.58-r0",
    sha256: "1eec55004cf7167c914c8839f46691d2bc82767e20466bff51eb45d06f8e90cc",
    size: 44_534_524,
};

/// What the build calls the package.
pub const WHAT: &str = "kernel package";

impl Kernel {
    /// The pinned package of this architecture.
    pub fn of(arch: Arch) -> &'static Kernel {
        match arch {
            Arch::Aarch64 => &AARCH64,
            Arch::X86_64 => &X86_64,
        }
    }

    pub fn pin(&self) -> Pin<'static> {
        Pin {
            size: self.size,
            sha256: self.sha256,
        }
    }

    /// The tag of our copy. arm64's has no architecture in it: it was published before there was another.
    pub fn tag(&self) -> String {
        match self.arch {
            Arch::Aarch64 => format!("kernel-linux-virt-{}", self.version),
            arch => format!("kernel-linux-virt-{}-{}", arch.name(), self.version),
        }
    }

    /// What messages call it.
    pub fn what(&self) -> String {
        format!("{} {WHAT}", self.arch.name())
    }

    /// The kernel's file name in the bundle and in a release.
    pub fn file_name(&self) -> &'static str {
        match self.arch {
            Arch::Aarch64 => "Image",
            Arch::X86_64 => "vmlinux",
        }
    }

    /// Where a build reads the pinned package, in order: our copy, then Alpine.
    pub fn sources(&self) -> [Source; 2] {
        let file = format!("linux-virt-{}.apk", self.version);
        [
            Source {
                name: "our copy",
                url: format!(
                    "https://github.com/codefusion-cc/runner/releases/download/{}/{file}",
                    self.tag()
                ),
                hosts: GITHUB_RELEASE_HOSTS,
            },
            Source {
                name: "Alpine",
                url: format!(
                    "https://dl-cdn.alpinelinux.org/alpine/{ALPINE_BRANCH}/main/{}/{file}",
                    self.arch.name()
                ),
                hosts: &["dl-cdn.alpinelinux.org"],
            },
        ]
    }

    /// Alpine's index of the branch the package comes from.
    fn alpine_index(&self) -> Source {
        Source {
            name: "Alpine's index",
            url: format!(
                "https://dl-cdn.alpinelinux.org/alpine/{ALPINE_BRANCH}/main/{}/APKINDEX.tar.gz",
                self.arch.name()
            ),
            hosts: &["dl-cdn.alpinelinux.org"],
        }
    }

    /// The plain kernel the monitors boot, from the package's `boot/vmlinuz-virt`.
    pub fn unpack(&self, vmlinuz: &[u8]) -> Result<Vec<u8>> {
        match self.arch {
            Arch::Aarch64 => unpack_zboot(vmlinuz),
            Arch::X86_64 => unpack_bzimage(vmlinuz),
        }
    }
}

/// `cargo xtask kernel check`: fails only when our copy of a pinned package does not give the pinned bytes, since a
/// build would then live on a URL Alpine deletes with the next patch. Alpine slow, down or newer is a warning. Every
/// architecture is checked before it fails.
pub fn check(root: &Path, args: &[String]) -> Result<()> {
    crate::no_options(args)?;
    let mut failed = Vec::new();
    for arch in Arch::ALL {
        let kernel = Kernel::of(arch);
        let dir = Scratch::new(root.join("target/kernel-check").join(arch.name()))?;
        let checked = check_sources(
            &Curl::https(),
            kernel,
            &kernel.sources(),
            kernel.pin(),
            &kernel.alpine_index(),
            dir.path(),
        );
        if let Err(error) = pinned::conclude(checked, &kernel.what(), "xtask/src/kernel.rs") {
            failed.push(error.to_string());
        }
    }
    match failed.is_empty() {
        true => Ok(()),
        false => Err(failed.join("\n").into()),
    }
}

fn check_sources(
    curl: &Curl,
    kernel: &Kernel,
    sources: &[Source],
    pin: Pin<'_>,
    index: &Source,
    dir: &Path,
) -> Checked {
    let what = kernel.what();
    let (arch, version) = (kernel.arch.name(), kernel.version);
    let mut checked = pinned::check_sources(curl, sources, pin, dir, &what);
    match pinned::newest(curl, index, dir, |bytes| {
        newest_in_index(bytes).map_err(|e| e.to_string())
    }) {
        Ok(newest) if newest == version => {
            println!("Alpine {ALPINE_BRANCH} has no newer linux-virt for {arch}")
        }
        Ok(newest) => checked.notices.push(format!(
            "Alpine {ALPINE_BRANCH} has linux-virt {newest} for {arch}, the pin is {version}: move the pin \
             (xtask/src/kernel.rs)"
        )),
        Err(error) => checked
            .notices
            .push(format!("could not read Alpine's index for {arch}: {error}")),
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

fn le16(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn le32(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
}

fn le64(bytes: &[u8], at: usize) -> Option<usize> {
    usize::try_from(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
    .ok()
}

/// The ELF machine of x86_64.
const EM_X86_64: usize = 62;
/// The note that gives a kernel's 32-bit PVH entry point (`XEN_ELFNOTE_PHYS32_ENTRY`), which cloud-hypervisor boots.
const PVH_NOTE: usize = 18;

/// Unpacks an x86 bzImage (boot protocol 2.08 or newer, a gzip payload) to the `vmlinux` ELF inside, and refuses
/// one cloud-hypervisor could not boot: not a 64-bit x86_64 ELF, or without the PVH entry note.
pub fn unpack_bzimage(image: &[u8]) -> Result<Vec<u8>> {
    if image.get(0x202..0x206) != Some(b"HdrS") {
        return Err("the kernel is not a bzImage".into());
    }
    if le16(image, 0x206).is_none_or(|protocol| protocol < 0x0208) {
        return Err("the bzImage's boot protocol is older than 2.08, which says where its payload is".into());
    }
    let setup_sectors = match image[0x1f1] {
        0 => 4,
        sectors => sectors as usize,
    };
    let (offset, size) = (le32(image, 0x248), le32(image, 0x24c));
    let payload = offset
        .zip(size)
        .and_then(|(offset, size)| {
            let start = (setup_sectors + 1) * 512 + offset;
            image.get(start..start.checked_add(size)?)
        })
        .ok_or("the bzImage payload is cut off")?;
    if !payload.starts_with(&[0x1f, 0x8b]) {
        return Err("the bzImage payload is not compressed with gzip".into());
    }
    let mut kernel = Vec::new();
    GzDecoder::new(payload).read_to_end(&mut kernel)?;
    if !kernel.starts_with(b"\x7fELF\x02\x01") || le16(&kernel, 18) != Some(EM_X86_64) {
        return Err("the unpacked kernel is not a 64-bit x86_64 ELF".into());
    }
    if !has_pvh_note(&kernel) {
        return Err(
            "the unpacked kernel has no PVH entry note (CONFIG_PVH), which cloud-hypervisor boots".into(),
        );
    }
    Ok(kernel)
}

/// Whether a 64-bit little-endian ELF has a `Xen` note of type [`PVH_NOTE`] in one of its note segments.
fn has_pvh_note(elf: &[u8]) -> bool {
    let (Some(table), Some(entry), Some(count)) = (le64(elf, 32), le16(elf, 54), le16(elf, 56)) else {
        return false;
    };
    (0..count).any(|index| {
        let header = table + index * entry;
        if le32(elf, header) != Some(4) {
            return false;
        }
        let (Some(start), Some(size)) = (le64(elf, header + 8), le64(elf, header + 32)) else {
            return false;
        };
        let Some(notes) = start.checked_add(size).and_then(|end| elf.get(start..end)) else {
            return false;
        };
        let mut at = 0;
        while let (Some(name_size), Some(desc_size), Some(kind)) =
            (le32(notes, at), le32(notes, at + 4), le32(notes, at + 8))
        {
            let name = notes.get(at + 12..at + 12 + name_size);
            if kind == PVH_NOTE && name == Some(b"Xen\0".as_slice()) {
                return true;
            }
            at += 12 + name_size.next_multiple_of(4) + desc_size.next_multiple_of(4);
        }
        false
    })
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
            index_archive(&format!("P:linux-virt\nV:{}\n", AARCH64.version))
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
                &AARCH64,
                &[source(good, "/copy"), source(stalled, "/alpine")],
                pin(&hash),
                &index_source(stalled, "/APKINDEX.tar.gz"),
                dir.path(),
            );
            assert_eq!(checked.copy, Ok(()));
            assert_eq!(checked.notices.len(), 2, "{:?}", checked.notices);
            assert!(
                checked.notices[0].starts_with("test does not give the pinned aarch64 kernel package"),
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
                &AARCH64,
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
                &AARCH64,
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
                    &AARCH64,
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
        fn the_sources_are_https_and_name_the_pinned_version_of_each_arch() {
            for arch in Arch::ALL {
                let kernel = Kernel::of(arch);
                assert_eq!(kernel.arch, arch);
                let [ours, alpine] = kernel.sources();
                for source in [&ours, &alpine, &kernel.alpine_index()] {
                    assert!(source.url.starts_with("https://"), "{}", source.url);
                    assert!(source.hosts.contains(&host_of(&source.url).unwrap().as_str()));
                }
                for source in [&ours, &alpine] {
                    let file = format!("linux-virt-{}.apk", kernel.version);
                    assert!(source.url.ends_with(&file), "{}", source.url);
                }
                assert!(
                    alpine.url.contains(&format!("/main/{}/", arch.name())),
                    "{}",
                    alpine.url
                );
                assert!(ours.url.contains(&format!("/download/{}/", kernel.tag())));
                assert_eq!(kernel.sha256.len(), 64);
                assert!(
                    kernel
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                );
            }
            // arm64's copy keeps the tag it was published under; another architecture's names itself.
            assert_eq!(AARCH64.tag(), format!("kernel-linux-virt-{}", AARCH64.version));
            assert_eq!(
                X86_64.tag(),
                format!("kernel-linux-virt-x86_64-{}", X86_64.version)
            );
            assert_ne!(AARCH64.file_name(), X86_64.file_name());
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

    /// A 64-bit ELF of `machine` with one note segment holding a note of `kind` named `name`.
    fn elf(machine: u16, name: &[u8], kind: u32) -> Vec<u8> {
        let mut elf = vec![0u8; 64 + 56];
        elf[0..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[18..20].copy_from_slice(&machine.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        let mut note = Vec::new();
        note.extend((name.len() as u32).to_le_bytes());
        note.extend(4u32.to_le_bytes());
        note.extend(kind.to_le_bytes());
        note.extend(name);
        note.resize(note.len().next_multiple_of(4), 0);
        note.extend(0x0100_0000u32.to_le_bytes());
        elf[64..68].copy_from_slice(&4u32.to_le_bytes());
        let notes_at = elf.len() as u64;
        elf[72..80].copy_from_slice(&notes_at.to_le_bytes());
        elf[96..104].copy_from_slice(&(note.len() as u64).to_le_bytes());
        elf.extend(note);
        elf
    }

    fn vmlinux() -> Vec<u8> {
        elf(62, b"Xen\0", 18)
    }

    /// A bzImage of `setup_sectors` (0 meaning 4) whose payload, at `gap` bytes past the setup, is `payload`.
    fn bzimage(setup_sectors: u8, gap: usize, payload: &[u8]) -> Vec<u8> {
        let start = (if setup_sectors == 0 {
            4
        } else {
            setup_sectors as usize
        } + 1)
            * 512;
        let mut image = vec![0u8; start + gap];
        image[0x1f1] = setup_sectors;
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
        image[0x248..0x24c].copy_from_slice(&(gap as u32).to_le_bytes());
        image[0x24c..0x250].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        image.extend(payload);
        image.extend([0u8; 4096]);
        image
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(bytes).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn a_bzimage_unpacks_to_its_vmlinux() {
        for (sectors, gap) in [(1, 0), (0, 300), (27, 17)] {
            assert_eq!(
                unpack_bzimage(&bzimage(sectors, gap, &gzip(&vmlinux()))).unwrap(),
                vmlinux(),
                "{sectors} {gap}"
            );
        }
        assert_eq!(
            X86_64.unpack(&bzimage(1, 0, &gzip(&vmlinux()))).unwrap(),
            vmlinux()
        );
        assert!(AARCH64.unpack(&bzimage(1, 0, &gzip(&vmlinux()))).is_err());
        assert!(X86_64.unpack(&zboot(&kernel(), b"gzip\0\0\0\0")).is_err());
    }

    #[test]
    fn a_bzimage_cloud_hypervisor_could_not_boot_is_refused() {
        let refused = |image: Vec<u8>, why: &str| {
            let error = unpack_bzimage(&image).unwrap_err().to_string();
            assert!(error.contains(why), "{error}, wanted {why}");
        };
        refused(b"short".to_vec(), "not a bzImage");
        refused(zboot(&kernel(), b"gzip\0\0\0\0"), "not a bzImage");
        let mut old = bzimage(1, 0, &gzip(&vmlinux()));
        old[0x206..0x208].copy_from_slice(&0x0207u16.to_le_bytes());
        refused(old, "older than 2.08");
        let mut lying = bzimage(1, 0, &gzip(&vmlinux()));
        lying[0x24c..0x250].copy_from_slice(&u32::MAX.to_le_bytes());
        refused(lying, "cut off");
        refused(bzimage(1, 0, &vmlinux()), "not compressed with gzip");
        let mut broken = gzip(&vmlinux());
        broken.truncate(broken.len() / 2);
        assert!(unpack_bzimage(&bzimage(1, 0, &broken)).is_err());
        refused(
            bzimage(1, 0, &gzip(&elf(183, b"Xen\0", 18))),
            "not a 64-bit x86_64 ELF",
        );
        let mut narrow = vmlinux();
        narrow[4] = 1;
        refused(bzimage(1, 0, &gzip(&narrow)), "not a 64-bit x86_64 ELF");
        refused(bzimage(1, 0, &gzip(&elf(62, b"Xen\0", 17))), "no PVH entry note");
        refused(
            bzimage(1, 0, &gzip(&elf(62, b"Linux\0", 18))),
            "no PVH entry note",
        );
        let mut no_notes = vmlinux();
        no_notes[64..68].copy_from_slice(&1u32.to_le_bytes());
        refused(bzimage(1, 0, &gzip(&no_notes)), "no PVH entry note");
        let mut past_the_end = vmlinux();
        past_the_end[96..104].copy_from_slice(&u64::MAX.to_le_bytes());
        refused(bzimage(1, 0, &gzip(&past_the_end)), "no PVH entry note");
    }
}
