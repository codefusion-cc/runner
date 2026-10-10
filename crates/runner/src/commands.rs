use std::path::PathBuf;
use std::time::Duration;

use cf_image::{DirFiles, TRUSTED_PUBLIC_KEY, parse_public_key, verify, verify_embedded};

const USAGE: &str = "usage: codefusion-runner version | image verify <folder> --version <v> | boot-proof [--memory-mb N] [--timeout-s N]";

pub fn run(args: &[String]) -> Result<(), String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["version"] | ["--version"] => {
            println!("codefusion-runner {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ["image", "verify", folder, "--version", version] => verify_folder(folder, version),
        ["boot-proof", rest @ ..] => boot_proof(rest),
        _ => Err(USAGE.into()),
    }
}

/// Checks a downloaded release folder against the committed public key. The command keeps nothing between runs,
/// so it cannot say whether the release is older than one accepted before.
fn verify_folder(folder: &str, version: &str) -> Result<(), String> {
    let key = parse_public_key(TRUSTED_PUBLIC_KEY).map_err(|error| error.to_string())?;
    let verified =
        verify(&DirFiles::new(folder), &key, version, 0).map_err(|error| format!("refused: {error}"))?;
    println!("ok: image {} verified", verified.manifest.version);
    Ok(())
}

#[cfg(target_os = "macos")]
fn boot_proof(options: &[&str]) -> Result<(), String> {
    let mut memory_mb = 1024u64;
    let mut timeout = 60u64;
    let mut iter = options.iter();
    while let Some(option) = iter.next() {
        let value = iter
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(USAGE)?;
        match *option {
            "--memory-mb" => memory_mb = value,
            "--timeout-s" => timeout = value,
            _ => return Err(USAGE.into()),
        }
    }
    if memory_mb == 0 || timeout == 0 {
        return Err(USAGE.into());
    }
    let memory_bytes = memory_mb
        .checked_mul(1024 * 1024)
        .ok_or("--memory-mb is too large")?;

    // The embedded image is verified before a byte of it reaches the hypervisor.
    let verified =
        verify_embedded().map_err(|error| format!("the embedded guest image is refused: {error}"))?;
    let bundle = cf_image::embedded_bundle().ok_or("no embedded guest image")?;
    let scratch = Scratch::new()?;
    let write = |name: &str| -> Result<PathBuf, String> {
        scratch.write(name, bundle.file(name).ok_or(name.to_string())?)
    };
    let spec = cf_vmm_vz::BootSpec {
        kernel: write(&verified.manifest.kernel.name)?,
        initramfs: write(&verified.manifest.initramfs.name)?,
        memory_bytes,
        cpus: 1,
        timeout: Duration::from_secs(timeout),
    };
    let report = cf_vmm_vz::boot_to_ready(&spec).map_err(|error| error.to_string())?;
    drop(scratch);

    print!("{}", report.console);
    println!(
        "image {}: ready after {:.2?}, stopped {:.2?} after the request ({})",
        verified.manifest.version,
        report.ready_after,
        report.stopped_after,
        if report.clean_poweroff {
            "guest powered itself off"
        } else {
            "stopped by the host"
        }
    );
    if report.clean_poweroff {
        Ok(())
    } else {
        Err("the guest did not power off by itself".into())
    }
}

/// A folder of this process alone for the proof's kernel and initramfs, removed when it goes out of scope, however
/// the proof ends. What the hypervisor loads from it is what was verified: nobody else can write there.
#[cfg(target_os = "macos")]
struct Scratch(PathBuf);

#[cfg(target_os = "macos")]
impl Scratch {
    fn new() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt as _;
        let path = std::env::temp_dir().join(format!("codefusion-runner-proof-{}", std::process::id()));
        // Made new and private in one step. A folder already there is not ours to trust, whoever left it.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self(path))
    }

    /// Writes a file that did not exist, readable by this user only.
    fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = self.0.join(name);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(bytes))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(path)
    }
}

#[cfg(target_os = "macos")]
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(not(target_os = "macos"))]
fn boot_proof(_options: &[&str]) -> Result<(), String> {
    Err("the boot proof runs on macOS only".into())
}
