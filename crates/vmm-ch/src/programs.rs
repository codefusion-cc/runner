//! The programs a machine runs beside the Runner. cloud-hypervisor the Runner carries ([`crate::pins`]): it is
//! unpacked into a private folder of the state folder and checked byte for byte before every machine, so what runs
//! is what was built in. pasta comes with the system (`apt install passt`): it is found on `PATH`, refused when
//! another user could change it, and tried once with the options a machine runs it with.

use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

/// cloud-hypervisor's static build for this architecture as this build carries it, if it carries it.
pub fn embedded() -> Option<&'static [u8]> {
    EMBEDDED
}

/// How pasta is installed, for the messages that ask for it.
pub const PASTA_INSTALL: &str = "apt install passt";
/// How long pasta's trial run may take.
pub const PASTA_TRIAL: Duration = Duration::from_secs(10);
/// Where pasta is looked for when there is no `PATH`.
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// What a build without cloud-hypervisor says, and how to make one with it.
pub const NOT_EMBEDDED: &str = "this build carries no cloud-hypervisor: build it with `cargo xtask build` on Linux for \
                                aarch64 or x86_64, the architectures it is pinned for";

/// Unpacks the embedded cloud-hypervisor into `dir` (made private when it is not there), checked byte for byte,
/// and says where it is; a file that is there and right stays. A folder or file another user could change is
/// refused.
pub fn unpack(dir: &Path) -> Result<PathBuf, String> {
    let embedded = embedded().ok_or(NOT_EMBEDDED)?;
    unpack_these(dir, embedded)
}

fn unpack_these(dir: &Path, program: &[u8]) -> Result<PathBuf, String> {
    let say = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
    cf_secret::private_dir(dir).map_err(|e| say(dir, e))?;
    owned_by(dir, false)?;
    let path = dir.join("cloud-hypervisor");
    if check(&path, program).is_ok() {
        return Ok(path);
    }
    // A link in its place is replaced by the rename, not followed.
    cf_secret::replace_private_as(&path, program, 0o500).map_err(|e| say(&path, e))?;
    Ok(path)
}

/// Whether the file at `path` (not what a link there leads to) is this user's, writable by nobody else, and holds
/// `wanted`, byte for byte.
fn check(path: &Path, wanted: &[u8]) -> Result<(), String> {
    owned_by(path, false)?;
    let say = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(say)?;
    let differs = || {
        Err(format!(
            "{} is not the program this build carries",
            path.display()
        ))
    };
    if file.metadata().map_err(say)?.len() != wanted.len() as u64 {
        return differs();
    }
    let mut chunk = vec![0u8; 64 << 10];
    let mut at = 0;
    while at < wanted.len() {
        let read = file.read(&mut chunk).map_err(say)?;
        if read == 0 || chunk[..read] != wanted[at..(at + read).min(wanted.len())] {
            return differs();
        }
        at += read;
    }
    Ok(())
}

/// pasta as this system has it: the first on `PATH`, which no other user can change and which runs with the
/// options a machine runs it with ([`crate::command::pasta`]).
pub fn pasta() -> Result<PathBuf, String> {
    let found = find("pasta", std::env::var_os("PATH").as_deref())?;
    trusted(&found)?;
    try_pasta(&found, PASTA_TRIAL)?;
    Ok(found)
}

/// The first file called `name` in the folders of `path` (a `PATH`; a default one when there is none).
fn find(name: &str, path: Option<&OsStr>) -> Result<PathBuf, String> {
    let path = path
        .filter(|path| !path.is_empty())
        .unwrap_or(OsStr::new(DEFAULT_PATH));
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| fs::metadata(candidate).is_ok_and(|found| found.is_file()))
        .ok_or_else(|| {
            format!("{name} is not installed (`{PASTA_INSTALL}`): machines with a network need it")
        })
}

/// Whether no user but root and this one can change what runs as `path`: a link there, the file it leads to, and
/// every folder above either.
fn trusted(path: &Path) -> Result<(), String> {
    trusted_below(path, Path::new("/"))
}

/// [`trusted`], for what is below `top` (`top` itself is not checked): the tests' files are in a temporary folder
/// that other users may write to.
fn trusted_below(path: &Path, top: &Path) -> Result<(), String> {
    let target = fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    for each in path
        .ancestors()
        .take_while(|each| *each != top)
        .chain(target.ancestors().take_while(|each| *each != top))
    {
        if !each.as_os_str().is_empty() {
            owned_by(each, true)?;
        }
    }
    if !fs::metadata(&target).is_ok_and(|found| found.is_file()) {
        return Err(format!("{} is no file", target.display()));
    }
    Ok(())
}

/// Runs pasta at `path` once as a machine runs it, with `true` in place of the sandbox, and says what it said if
/// it would not.
fn try_pasta(path: &Path, within: Duration) -> Result<(), String> {
    let mut command = crate::command::pasta(path, vec!["true".into()]);
    let program = command.remove(0);
    let mut child = Command::new(&program)
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let began = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if began.elapsed() < within => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut said = String::new();
    if let Some(mut errors) = child.stderr.take() {
        let _ = errors.by_ref().take(64 << 10).read_to_string(&mut said);
    }
    let first: Vec<&str> = said
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect();
    let said = if first.is_empty() {
        String::new()
    } else {
        format!(": {}", first.join(" | "))
    };
    let refused = |what: String| {
        Err(format!(
            "{} {what}; machines need pasta of passt 2023-03 or newer (`{PASTA_INSTALL}`){said}",
            path.display()
        ))
    };
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => refused(format!("would not run as a machine runs it ({status})")),
        None => refused(format!("did not end within {within:?} of a trial run")),
    }
}

/// Whether `path` (not what a link there leads to) belongs to this user, or also root when `root` is true, and no
/// other user may write to it.
fn owned_by(path: &Path, root: bool) -> Result<(), String> {
    let found = fs::symlink_metadata(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound => format!("{} is not there", path.display()),
        _ => format!("{}: {e}", path.display()),
    })?;
    // SAFETY: geteuid takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    let owner = found.uid() == me || (root && found.uid() == 0);
    let link = found.file_type().is_symlink();
    // A link's own mode means nothing; who owns it and the folder it is in say who can change it.
    let writable = !link && found.mode() & 0o022 != 0;
    if !owner || writable || (link && !root) {
        return Err(format!(
            "{} is not this user's {}alone, so it cannot be trusted",
            path.display(),
            if root { "and root's " } else { "" }
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    const FAKE: &[u8] = b"a monitor";

    /// A temporary folder, by the path without links that pasta's check walks up; checks stop above it.
    struct Private {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Private {
        fn path(&self) -> &Path {
            &self.path
        }

        fn top(&self) -> &Path {
            self.path.parent().unwrap()
        }
    }

    fn private_dir() -> Private {
        let dir = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        Private { _dir: dir, path }
    }

    fn mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A pasta that runs `script` with the arguments it is given.
    fn fake_pasta(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("pasta");
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        mode(&path, 0o755);
        path
    }

    #[test]
    fn cloud_hypervisor_is_unpacked_private_and_runnable() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().join("bin");
        let path = unpack_these(&dir, FAKE).unwrap();
        assert_eq!(path, dir.join("cloud-hypervisor"));
        assert_eq!(fs::read(&path).unwrap(), b"a monitor");
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o500);
        assert!(!dir.join("cloud-hypervisor.partial").exists());
    }

    #[test]
    fn a_changed_program_is_put_back_and_a_right_one_stays() {
        let folder = tempfile::tempdir().unwrap();
        let path = unpack_these(folder.path(), FAKE).unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        unpack_these(folder.path(), FAKE).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        mode(&path, 0o700);
        fs::write(&path, b"something else").unwrap();
        unpack_these(folder.path(), FAKE).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a monitor");
    }

    #[test]
    fn a_link_in_place_of_the_program_is_replaced_not_followed() {
        let folder = tempfile::tempdir().unwrap();
        let elsewhere = folder.path().join("elsewhere");
        fs::write(&elsewhere, b"a monitor").unwrap();
        let dir = folder.path().join("bin");
        fs::create_dir(&dir).unwrap();
        mode(&dir, 0o700);
        std::os::unix::fs::symlink(&elsewhere, dir.join("cloud-hypervisor")).unwrap();
        let path = unpack_these(&dir, FAKE).unwrap();
        assert!(!fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&elsewhere).unwrap(), b"a monitor");
    }

    #[test]
    fn a_folder_others_may_write_to_is_refused() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().join("bin");
        fs::create_dir(&dir).unwrap();
        mode(&dir, 0o777);
        let refused = unpack_these(&dir, FAKE).unwrap_err();
        assert!(refused.contains("cannot be trusted"), "{refused}");
    }

    #[test]
    fn a_check_takes_the_same_bytes_only() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("p");
        let program: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &program).unwrap();
        mode(&path, 0o500);
        assert!(check(&path, &program).is_ok());
        let mut changed = program.clone();
        *changed.last_mut().unwrap() ^= 1;
        let refused = check(&path, &changed).unwrap_err();
        assert!(
            refused.ends_with("is not the program this build carries"),
            "{refused}"
        );
        assert!(check(&path, &program[..program.len() - 1]).is_err());
        assert!(check(&path, b"").is_err());
        assert!(
            check(&folder.path().join("missing"), b"x")
                .unwrap_err()
                .contains("not there")
        );
    }

    #[test]
    fn pasta_is_the_first_on_the_path_and_missing_says_how_to_install_it() {
        let (first, second) = (private_dir(), private_dir());
        let wanted = fake_pasta(second.path(), "exit 0");
        // A folder of that name is no program, and a relative folder of the path is never looked in.
        fs::create_dir(first.path().join("pasta")).unwrap();
        let path = std::env::join_paths([Path::new("relative"), first.path(), second.path()]).unwrap();
        assert_eq!(find("pasta", Some(&path)).unwrap(), wanted);
        let earlier = std::env::join_paths([second.path(), first.path()]).unwrap();
        assert_eq!(find("pasta", Some(&earlier)).unwrap(), wanted);
        let missing = find("pasta", Some(first.path().as_os_str())).unwrap_err();
        assert_eq!(
            missing,
            "pasta is not installed (`apt install passt`): machines with a network need it"
        );
        assert!(find("pasta", Some(OsStr::new("relative"))).is_err());
    }

    #[test]
    fn pasta_that_runs_as_a_machine_runs_it_is_taken() {
        let dir = private_dir();
        // It is given every option a machine's pasta is, and the program after `--`.
        let pasta = fake_pasta(
            dir.path(),
            r#"for a; do last="$a"; done; [ "$last" = true ] && [ "$1" = --foreground ]"#,
        );
        trusted_below(&pasta, dir.top()).unwrap();
        try_pasta(&pasta, PASTA_TRIAL).unwrap();
    }

    #[test]
    fn pasta_too_old_for_an_option_says_so_with_its_own_words() {
        let dir = private_dir();
        let pasta = fake_pasta(
            dir.path(),
            "echo \"pasta: unrecognized option '--no-map-gw'\" >&2; echo Usage: >&2; echo a >&2; echo b >&2; exit 1",
        );
        let refused = try_pasta(&pasta, PASTA_TRIAL).unwrap_err();
        assert!(
            refused.contains("passt 2023-03 or newer (`apt install passt`)"),
            "{refused}"
        );
        assert!(
            refused.ends_with(": pasta: unrecognized option '--no-map-gw' | Usage: | a"),
            "{refused}"
        );
    }

    #[test]
    fn pasta_that_hangs_is_ended_and_refused() {
        let dir = private_dir();
        let pasta = fake_pasta(dir.path(), "exec sleep 30");
        let began = Instant::now();
        let refused = try_pasta(&pasta, Duration::from_millis(300)).unwrap_err();
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(refused.contains("did not end within 300ms"), "{refused}");
    }

    #[test]
    fn pasta_another_user_could_change_is_refused() {
        let dir = private_dir();
        let pasta = fake_pasta(dir.path(), "exit 0");
        trusted_below(&pasta, dir.top()).unwrap();
        mode(&pasta, 0o775);
        assert!(
            trusted_below(&pasta, dir.top())
                .unwrap_err()
                .contains("cannot be trusted")
        );
        mode(&pasta, 0o755);
        mode(dir.path(), 0o777);
        assert!(
            trusted_below(&pasta, dir.top())
                .unwrap_err()
                .contains("cannot be trusted")
        );
        mode(dir.path(), 0o700);
        // From the top of the tree, every folder counts: one of them here is the system's temporary folder.
        if fs::metadata(dir.top()).unwrap().mode() & 0o022 != 0 {
            assert!(trusted(&pasta).is_err());
        }
    }

    #[test]
    fn pasta_through_a_link_checks_the_link_and_what_it_leads_to() {
        let (links, files) = (private_dir(), private_dir());
        let top = links.top();
        assert_eq!(top, files.top());
        let passt = fake_pasta(files.path(), "exit 0");
        let link = links.path().join("pasta-link");
        std::os::unix::fs::symlink(&passt, &link).unwrap();
        trusted_below(&link, top).unwrap();
        // The folder of what the link leads to, then the link's own.
        for open in [&files, &links] {
            mode(open.path(), 0o777);
            let refused = trusted_below(&link, top).unwrap_err();
            assert_eq!(
                refused,
                format!(
                    "{} is not this user's and root's alone, so it cannot be trusted",
                    open.path().display()
                )
            );
            mode(open.path(), 0o700);
            trusted_below(&link, top).unwrap();
        }
        let dangling = links.path().join("dangling");
        std::os::unix::fs::symlink(files.path().join("gone"), &dangling).unwrap();
        assert!(trusted_below(&dangling, top).is_err());
    }
}
