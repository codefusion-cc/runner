//! A job's cache disk: a copy of its repository's cache, mounted at [`MOUNT`]. It holds npm's cache, pnpm's store and
//! Playwright's browsers, which the job's user writes, and a Docker job's daemon data. Nothing else is kept: the job's end deletes the rest
//! ([`leftovers`]) before the disk is let go, so a cache holds downloaded packages and images, never a job's logs or
//! programs that npx installed.

/// Where the guest mounts the cache disk.
pub const MOUNT: &str = "/mnt/cache";
/// A Docker job's daemon data, the daemon's own (root's).
pub const DOCKER: &str = "docker";
/// npm's cache, the job user's.
pub const NPM: &str = "npm";
/// pnpm's store, the job user's.
pub const PNPM: &str = "pnpm-store";
/// Playwright's browsers, the job user's: each in a folder of its own revision, which Playwright looks for itself.
pub const PLAYWRIGHT: &str = "ms-playwright";
/// What npm keeps in its cache folder that is worth keeping: the downloaded packages and their metadata.
pub const NPM_KEPT: &str = "_cacache";
/// The folders of the disk's top level that are kept; `lost+found` is the file system's own.
pub const KEPT: [&str; 5] = [DOCKER, NPM, PNPM, PLAYWRIGHT, "lost+found"];
/// The folders the job's user writes.
pub const USER_FOLDERS: [&str; 3] = [NPM, PNPM, PLAYWRIGHT];

/// What a job with a cache disk is told: where the disk is, and npm's cache, pnpm's store and Playwright's browsers
/// on it. The job's own
/// entries come after these and win ([`crate::environment::workload_environment`]).
pub fn environment(mount: &str) -> Vec<(String, String)> {
    vec![
        ("CF_JOB_CACHE".to_string(), mount.to_string()),
        ("npm_config_cache".to_string(), format!("{mount}/{NPM}")),
        ("pnpm_config_store_dir".to_string(), format!("{mount}/{PNPM}")),
        (
            "PLAYWRIGHT_BROWSERS_PATH".to_string(),
            format!("{mount}/{PLAYWRIGHT}"),
        ),
    ]
}

/// An entry of a folder on the cache: its name, and whether it is a folder itself (not a link to one).
pub type Entry = (String, bool);

/// What the job's end deletes, as paths under the mount: every top-level entry that is not a folder of [`KEPT`], and
/// everything in npm's folder but the folder [`NPM_KEPT`] (its logs, npx's installs, the update notifier's stamp).
/// A link where a kept folder goes is deleted, never followed: the guest's root does this, and a link would point it
/// anywhere. `top` and `npm` are the entries of the disk's top level and of npm's folder.
pub fn leftovers(top: &[Entry], npm: &[Entry]) -> Vec<String> {
    let kept = |(name, folder): &Entry, names: &[&str]| *folder && names.contains(&name.as_str());
    let mut gone: Vec<String> = top
        .iter()
        .filter(|entry| !kept(entry, &KEPT))
        .map(|(name, _)| name.clone())
        .collect();
    if top.iter().any(|entry| entry.0 == NPM && entry.1) {
        gone.extend(
            npm.iter()
                .filter(|entry| !kept(entry, &[NPM_KEPT]))
                .map(|(name, _)| format!("{NPM}/{name}")),
        );
    }
    gone
}
