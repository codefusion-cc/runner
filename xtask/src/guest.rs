//! The guest bundle: kernel, supervisor and modules packed into an initramfs, then signed as a release folder.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use cf_image::{DirFiles, ReleaseInputs, public_key_text, verify, write_release};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

use crate::cpio::{Archive, Kind};
use crate::kernel::{APK_SHA256, APK_VERSION, PIN, WHAT, sources, unpack_zboot};
use crate::keys::{ephemeral_key, release_key};
use crate::pinned::{Curl, ensure};
use crate::{Result, cargo, run_command};

/// The modules the guest loads at boot, with what they need: the machine's devices, the file systems of a job's
/// disks, and what its network and firewall take.
const MODULES: &[&str] = &[
    "virtio_blk",
    "virtio_net",
    "virtio-rng",
    "vmw_vsock_virtio_transport",
    "erofs",
    "ext4",
    "overlay",
    "af_packet",
    "nf_tables",
    "nft_ct",
];
/// A release asset may be at most 2 GiB; the root filesystem is cut a little below that.
const MAX_PART: u64 = 1_900_000_000;
/// The names of the kernel and the initramfs in the bundle and in a release.
const KERNEL_FILE: &str = "Image";
const INITRAMFS_FILE: &str = "initramfs.cpio.gz";

/// Builds the bundle in `target/guest`, signed with a key made for this build. The key that signs image releases
/// never signs a bundle: a bundle has no root filesystem and no build time, and must not pass as a release.
pub fn build(root: &Path) -> Result<()> {
    let target = root.join("target");
    let staging = target.join("guest-staging");
    let out = target.join("guest");
    for dir in [&staging, &out] {
        let _ = fs::remove_dir_all(dir);
        fs::create_dir_all(dir)?;
    }

    let package = fetch_package(&target.join("cache"))?;
    let kernel = unpack_zboot(&fs::read(package.join("boot/vmlinuz-virt"))?)?;
    fs::write(staging.join(KERNEL_FILE), &kernel)?;

    let modules = collect_modules(&package)?;
    run_command(Command::new(cargo()).current_dir(root).args([
        "build",
        "--release",
        "--target",
        "aarch64-unknown-linux-musl",
        "-p",
        "cf-supervisor",
    ]))?;
    let supervisor = fs::read(target.join("aarch64-unknown-linux-musl/release/supervisor"))?;
    let initramfs = initramfs(&supervisor, &modules)?;
    fs::write(staging.join(INITRAMFS_FILE), &initramfs)?;

    let version = format!(
        "guest-{APK_VERSION}-{}",
        &format!("{:x}", Sha256::digest(&initramfs))[..12]
    );
    let key = ephemeral_key()?;
    write_release(
        &out,
        &ReleaseInputs {
            version: &version,
            built: 0,
            kernel: &staging.join(KERNEL_FILE),
            initramfs: &staging.join(INITRAMFS_FILE),
            rootfs: None,
            max_part: MAX_PART,
        },
        &key,
    )?;
    // The embedding build reads the key from here (cf-image/build.rs).
    fs::write(out.join("bundle-key.txt"), format!("{}\n", public_key_text(&key)))?;
    println!("guest bundle {version} in {}", out.display());
    Ok(())
}

/// `release --version V --out DIR --rootfs FILE (--key-file F | --ephemeral) [--guest DIR] [--max-part BYTES]`:
/// the folder the image workflow publishes, verified as a machine would verify it before this returns.
pub fn release(root: &Path, args: &[String]) -> Result<()> {
    let (mut version, mut out, mut key_file, mut rootfs) = (None, None, None, None);
    let (mut guest, mut max_part, mut ephemeral) = (root.join("target/guest"), MAX_PART, false);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--ephemeral" {
            ephemeral = true;
            continue;
        }
        let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--version" => version = Some(value.clone()),
            "--out" => out = Some(PathBuf::from(value)),
            "--key-file" => key_file = Some(PathBuf::from(value)),
            "--guest" => guest = PathBuf::from(value),
            "--rootfs" => rootfs = Some(PathBuf::from(value)),
            "--max-part" => {
                max_part = value
                    .parse()
                    .ok()
                    .filter(|bytes| *bytes > 0)
                    .ok_or("--max-part needs a number of bytes above 0")?;
            }
            other => return Err(format!("unknown option {other}").into()),
        }
    }
    let (version, out, rootfs) = (
        version.ok_or("--version is needed")?,
        out.ok_or("--out is needed")?,
        rootfs.ok_or("--rootfs is needed")?,
    );
    let key = release_key(key_file.as_deref(), ephemeral)?;
    let built = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let manifest = write_release(
        &out,
        &ReleaseInputs {
            version: &version,
            built,
            kernel: &guest.join(KERNEL_FILE),
            initramfs: &guest.join(INITRAMFS_FILE),
            rootfs: Some(&rootfs),
            max_part,
        },
        &key,
    )?;
    // Nothing leaves this task that the verification would refuse.
    let verified = verify(&DirFiles::new(&out), &key.verifying_key(), &version, built)?;
    if verified.manifest != manifest {
        return Err("the release folder verifies as another release than the one written".into());
    }
    println!(
        "release {} with {} files in {}, verified",
        manifest.version,
        manifest.assets().len() + 3,
        out.display()
    );
    Ok(())
}

/// Takes the pinned kernel package from the cache or else from its sources, unpacks it, and returns the folder.
///
/// A folder unpacked earlier is used as it is, without hashing it again: `target/` is trusted like the sources. A
/// workflow that restores `target/` from a cache must therefore never sign what it builds.
fn fetch_package(cache: &Path) -> Result<PathBuf> {
    fs::create_dir_all(cache)?;
    let apk = cache.join(format!("linux-virt-{APK_VERSION}.apk"));
    let unpacked = cache.join(format!("linux-virt-{}", &APK_SHA256[..12]));
    if unpacked.join("boot/vmlinuz-virt").exists() {
        return Ok(unpacked);
    }
    ensure(&Curl::https(), &sources(), PIN, &apk, WHAT)?;
    let partial = cache.join("unpack.partial");
    let _ = fs::remove_dir_all(&partial);
    fs::create_dir_all(&partial)?;
    // An apk is a few gzip streams of one tar; the first holds the signature and tar reads on past it. GNU tar
    // warns about apk's own header on every file, so what tar says is shown only when it fails.
    let unpack = Command::new("tar")
        .arg("-xzf")
        .arg(&apk)
        .arg("-C")
        .arg(&partial)
        .output()?;
    if !unpack.status.success() {
        return Err(format!(
            "tar could not unpack the kernel package: {}",
            String::from_utf8_lossy(&unpack.stderr)
        )
        .into());
    }
    let _ = fs::remove_dir_all(&unpacked);
    fs::rename(&partial, &unpacked)?;
    Ok(unpacked)
}

/// The wanted modules and what they depend on, unpacked, in the order to load them: `(name, bytes)`.
fn collect_modules(package: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let release_dir = fs::read_dir(package.join("lib/modules"))?
        .next()
        .ok_or("no modules in the package")??
        .path();
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in fs::read_to_string(release_dir.join("modules.dep"))?.lines() {
        let (module, needs) = line
            .split_once(':')
            .ok_or("modules.dep has a line with no colon")?;
        deps.insert(
            module.to_string(),
            needs.split_whitespace().map(str::to_string).collect(),
        );
    }
    let find = |name: &str| -> Result<String> {
        deps.keys()
            .find(|path| path.rsplit('/').next() == Some(&format!("{name}.ko.gz")))
            .cloned()
            .ok_or_else(|| format!("module {name} is not in the kernel package").into())
    };
    let mut order: Vec<String> = Vec::new();
    fn visit(path: &str, deps: &BTreeMap<String, Vec<String>>, order: &mut Vec<String>) -> Result<()> {
        if order.iter().any(|seen| seen == path) {
            return Ok(());
        }
        for needed in deps
            .get(path)
            .ok_or_else(|| format!("{path} is not in modules.dep"))?
        {
            visit(needed, deps, order)?;
        }
        order.push(path.to_string());
        Ok(())
    }
    for name in MODULES {
        visit(&find(name)?, &deps, &mut order)?;
    }
    order
        .into_iter()
        .map(|path| {
            let name = path
                .rsplit('/')
                .next()
                .unwrap()
                .trim_end_matches(".ko.gz")
                .to_string();
            let mut raw = Vec::new();
            GzDecoder::new(fs::File::open(release_dir.join(&path))?).read_to_end(&mut raw)?;
            Ok((name, raw))
        })
        .collect()
}

/// The initramfs: the supervisor as `/init`, the module list and the modules. Reproducible: same inputs, same bytes.
fn initramfs(supervisor: &[u8], modules: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut archive = Archive::new(Vec::new());
    for dir in ["dev", "proc", "sys", "etc", "lib", "lib/modules"] {
        archive.add(dir, Kind::Dir, 0o755)?;
    }
    archive.add("init", Kind::File(supervisor), 0o755)?;
    let list: String = modules.iter().map(|(name, _)| format!("{name}\n")).collect();
    archive.add("etc/modules", Kind::File(list.as_bytes()), 0o644)?;
    for (name, bytes) in modules {
        archive.add(&format!("lib/modules/{name}.ko"), Kind::File(bytes), 0o644)?;
    }
    let cpio = archive.finish()?;
    let mut gz = GzEncoder::new(Vec::new(), Compression::best());
    gz.write_all(&cpio)?;
    Ok(gz.finish()?)
}
