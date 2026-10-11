//! Tells the program which commit it was built at, for `version`, the first line of the log and the console:
//! twelve characters of the commit, with `-dirty` when what goes into the program differs from it. `CF_COMMIT`
//! names it for a build outside a checkout; without either it is `unknown`.

use std::path::Path;
use std::process::Command;

/// What goes into the program, as git names it: every crate without its tests, and the workspace's own files.
const BUILT_FROM: [&str; 4] = [
    "crates",
    ":(glob,exclude)crates/*/tests/**",
    "Cargo.toml",
    "Cargo.lock",
];

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| text.trim().to_string())
}

fn commit(root: &Path) -> Option<String> {
    let commit = git(root, &["rev-parse", "--short=12", "HEAD"]).filter(|commit| !commit.is_empty())?;
    let mut changed = vec!["status", "--porcelain", "--untracked-files=no", "--"];
    changed.extend(BUILT_FROM);
    // A change nobody could look for is no change anybody knows of.
    let dirty = git(root, &changed).is_some_and(|changed| !changed.is_empty());
    Some(if dirty { format!("{commit}-dirty") } else { commit })
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    println!("cargo:rerun-if-env-changed=CF_COMMIT");
    // Every commit, checkout and pull writes to these files of the checkout's own, so a new commit is a new build.
    // One that is not there is left out: cargo would take it for changed at every build.
    for file in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-path", file],
        ) && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    // And a change to what goes into the program is one too, so that `-dirty` is said, and taken back.
    for file in ["Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={}", root.join(file).display());
    }
    let crates = std::fs::read_dir(root.join("crates"))
        .into_iter()
        .flatten()
        .flatten();
    for part in crates.flat_map(|one| std::fs::read_dir(one.path()).into_iter().flatten().flatten()) {
        if part.file_name() != "tests" {
            println!("cargo:rerun-if-changed={}", part.path().display());
        }
    }
    let commit = std::env::var("CF_COMMIT")
        .ok()
        .map(|commit| commit.trim().to_string())
        .filter(|commit| !commit.is_empty())
        .or_else(|| commit(&root))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=CF_COMMIT={commit}");
}
