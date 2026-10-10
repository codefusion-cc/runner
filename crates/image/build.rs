//! Embeds the kernel, the initramfs and their signed manifest that `cargo xtask guest` left in `target/guest`
//! (or in `$CF_GUEST_DIR`). Without them the build still works and carries no image.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

const NO_IMAGE: &str = "const EMBEDDED: Option<Bundle> = None;\n";

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dir = env::var_os("CF_GUEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../target/guest"));
    println!("cargo:rerun-if-env-changed=CF_GUEST_DIR");
    println!("cargo:rerun-if-changed={}", dir.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("embedded.rs");
    let manifest = fs::read(dir.join("image.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let code = match manifest {
        Some(value) if dir.join("SHA256SUMS.sig").exists() => {
            let version = value["version"]
                .as_str()
                .expect("image.json has a version")
                .to_string();
            let key_file = dir.join("bundle-key.txt");
            println!("cargo:rerun-if-changed={}", key_file.display());
            let key = fs::read_to_string(&key_file)
                .unwrap_or_else(|_| panic!("bundle-key.txt is missing in {}", dir.display()));
            let names = [
                "image.json",
                "SHA256SUMS",
                "SHA256SUMS.sig",
                value["kernel"]["name"].as_str().unwrap(),
                value["initramfs"]["name"].as_str().unwrap(),
            ];
            let mut files = String::new();
            for name in names {
                let path = fs::canonicalize(dir.join(name))
                    .unwrap_or_else(|_| panic!("{name} is missing in {}", dir.display()));
                println!("cargo:rerun-if-changed={}", path.display());
                writeln!(
                    files,
                    "({name:?}, include_bytes!({:?}).as_slice()),",
                    path.display().to_string()
                )
                .unwrap();
            }
            format!(
                "const EMBEDDED: Option<Bundle> = Some(Bundle {{ version: {version:?}, key: {:?}, files: &[{files}] }});\n",
                key.trim()
            )
        }
        Some(_) => {
            println!(
                "cargo:warning={} has image.json but no SHA256SUMS.sig; building without an embedded guest image",
                dir.display()
            );
            NO_IMAGE.to_string()
        }
        None => NO_IMAGE.to_string(),
    };
    fs::write(out, code).unwrap();
}
