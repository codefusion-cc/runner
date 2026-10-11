//! Tells the program which commit it was built at, for `version` and the first line of the log. `CF_COMMIT` names
//! it for a build outside a checkout; without either it is `unknown`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    (output.status.success() && !text.trim().is_empty()).then(|| text.trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=CF_COMMIT");
    // Every commit, checkout and pull writes to these files of the checkout's own, so a new commit is a new build.
    // One that is not there is left out: cargo would take it for changed at every build.
    for file in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", file])
            && std::path::Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let commit = std::env::var("CF_COMMIT")
        .ok()
        .filter(|commit| !commit.trim().is_empty())
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=CF_COMMIT={}", commit.trim());
}
