//! Embeds cloud-hypervisor, which `cargo xtask cloud-hypervisor` left in `target/vmm-programs` (or in
//! `$CF_VMM_PROGRAMS_DIR`), into a build for Linux on an architecture it is pinned for (`src/pins.rs`: aarch64 and
//! x86_64). It must be the file pinned for the architecture of the build. Without it, or for another system or
//! architecture, the build still works and carries no monitor.

use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;

include!("src/pins.rs");

const NONE: &str = "const EMBEDDED: Option<&[u8]> = None;\n";

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dir = env::var_os("CF_VMM_PROGRAMS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../target/vmm-programs"));
    println!("cargo:rerun-if-env-changed=CF_VMM_PROGRAMS_DIR");
    println!("cargo:rerun-if-changed={}", dir.display());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("embedded.rs");
    let linux = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let pin = cloud_hypervisor_for(&arch).filter(|_| linux);
    let ch = dir.join("cloud-hypervisor");
    let code = match pin {
        Some(_) if !ch.exists() => {
            println!(
                "cargo:warning=no cloud-hypervisor in {}: building without it (cargo xtask cloud-hypervisor)",
                dir.display()
            );
            NONE.to_string()
        }
        None => NONE.to_string(),
        Some(pin) => {
            let ch = fs::canonicalize(&ch).unwrap();
            println!("cargo:rerun-if-changed={}", ch.display());
            let bytes = fs::read(&ch).unwrap();
            let hash = format!("{:x}", Sha256::digest(&bytes));
            assert!(
                bytes.len() as u64 == pin.size && hash == pin.sha256,
                "{} is not the pinned cloud-hypervisor {CLOUD_HYPERVISOR_VERSION} for {arch} ({}): \
                 cargo xtask cloud-hypervisor fetches the host's",
                ch.display(),
                pin.asset
            );
            format!(
                "const EMBEDDED: Option<&[u8]> = Some(include_bytes!({:?}));\n",
                ch.display().to_string()
            )
        }
    };
    fs::write(out, code).unwrap();
}
