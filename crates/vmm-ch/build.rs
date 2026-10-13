//! Embeds cloud-hypervisor and pasta, which `cargo xtask vmm-programs` left in `target/vmm-programs` (or in
//! `$CF_VMM_PROGRAMS_DIR`), into a build for Linux. cloud-hypervisor must be the pinned file; pasta, built here
//! from the pinned source, is recorded by its SHA-256, which the Runner checks it against when it unpacks it.
//! Without them, or for another system, the build still works and carries no programs.

use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;

include!("src/pins.rs");

const NONE: &str = "const EMBEDDED: Option<Embedded> = None;\n";

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dir = env::var_os("CF_VMM_PROGRAMS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../target/vmm-programs"));
    println!("cargo:rerun-if-env-changed=CF_VMM_PROGRAMS_DIR");
    println!("cargo:rerun-if-changed={}", dir.display());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("embedded.rs");
    let linux = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux");
    let (ch, pasta) = (dir.join("cloud-hypervisor"), dir.join("pasta"));
    let code = if !linux || !ch.exists() || !pasta.exists() {
        if linux {
            println!(
                "cargo:warning=no cloud-hypervisor and pasta in {}: building without them (cargo xtask vmm-programs)",
                dir.display()
            );
        }
        NONE.to_string()
    } else {
        let ch = fs::canonicalize(&ch).unwrap();
        let pasta = fs::canonicalize(&pasta).unwrap();
        println!("cargo:rerun-if-changed={}", ch.display());
        println!("cargo:rerun-if-changed={}", pasta.display());
        let bytes = fs::read(&ch).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        assert!(
            bytes.len() as u64 == CLOUD_HYPERVISOR_SIZE && hash == CLOUD_HYPERVISOR_SHA256,
            "{} is not the pinned cloud-hypervisor {CLOUD_HYPERVISOR_VERSION}",
            ch.display()
        );
        let pasta_sha256 = format!("{:x}", Sha256::digest(fs::read(&pasta).unwrap()));
        format!(
            "const EMBEDDED: Option<Embedded> = Some(Embedded {{ cloud_hypervisor: include_bytes!({:?}), \
             cloud_hypervisor_sha256: {CLOUD_HYPERVISOR_SHA256:?}, pasta: include_bytes!({:?}), \
             pasta_sha256: {pasta_sha256:?} }});\n",
            ch.display().to_string(),
            pasta.display().to_string()
        )
    };
    fs::write(out, code).unwrap();
}
