//! How the Runner updates itself on a Mac that built it: from the git checkout it was built from, with the toolchain
//! it was built with (`build.rs` writes both into the program). The console's `update` reaches it through
//! [`cf_daemon::Service`], which decides when to build and when to install; this module does the two, and the guard
//! at the start of `daemon` that counts the starts of a new program and puts the one before back.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use cf_daemon::update::{AtStart, Pending, SelfUpdate, UpdateFile, at_start, spoken};

use crate::commands::VERSION;

/// The branch updates come from.
const BRANCH: &str = "main";
/// How long a build may take before it is ended: the guest and the release build take some ten minutes on a Mac
/// that runs jobs beside it.
const BUILD_LIMIT: Duration = Duration::from_secs(45 * 60);
/// How long one git call may take.
const GIT_LIMIT: Duration = Duration::from_secs(120);
/// The name the kept program has beside the installed one.
const PREVIOUS: &str = "prev";

/// The commit part of a version: `0.1.0+446125c0a1b2` → `446125c0a1b2`.
fn commit_of(version: &str) -> Option<&str> {
    version.split_once('+').map(|(_, commit)| commit)
}

/// The update of a Runner built from a checkout on this Mac.
pub struct MacUpdate {
    /// The checkout the running program was built from.
    pub checkout: PathBuf,
    /// The toolchain's `cargo` (rustup's, which takes the toolchain `rust-toolchain.toml` names).
    pub cargo: PathBuf,
    /// The build's whole environment: nothing of the daemon's own reaches it.
    pub env: Vec<(String, OsString)>,
    /// Where the build writes what it says.
    pub log: PathBuf,
    /// The installed program, which launchd runs.
    pub installed: PathBuf,
    /// The folder of the machines' records: an install is refused while one is there.
    pub records: PathBuf,
    /// The state folder, where [`UpdateFile`] is.
    pub state_dir: PathBuf,
    /// The running version.
    pub running: String,
    /// How long a build may take.
    pub build_limit: Duration,
    /// Whether a program may start virtual machines.
    pub entitled: fn(&Path) -> Result<(), String>,
    /// `<origin> main`, for the console.
    source: String,
}

impl MacUpdate {
    /// The update of the running program, from where `build.rs` says it was built. `None` when that is not a git
    /// checkout on this Mac, such as for a release built elsewhere.
    pub fn of_this_program(
        state_dir: &Path,
        installed: &Path,
        records: &Path,
        entitled: fn(&Path) -> Result<(), String>,
    ) -> Option<MacUpdate> {
        let checkout = PathBuf::from(option_env!("CF_SOURCE").filter(|s| !s.is_empty())?);
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let or_home = |built: Option<&str>, below: &str| {
            built
                .filter(|path| !path.is_empty())
                .map_or_else(|| home.join(below), PathBuf::from)
        };
        let cargo_home = or_home(option_env!("CF_CARGO_HOME"), ".cargo");
        let rustup_home = or_home(option_env!("CF_RUSTUP_HOME"), ".rustup");
        let mut env = vec![
            ("HOME".to_string(), home.clone().into_os_string()),
            ("PATH".to_string(), build_path(&cargo_home)),
            ("CARGO_HOME".to_string(), cargo_home.clone().into_os_string()),
            ("RUSTUP_HOME".to_string(), rustup_home.into_os_string()),
            // A Mac that runs jobs builds with fewer cores than it has.
            (
                "CARGO_BUILD_JOBS".to_string(),
                option_env!("CF_BUILD_JOBS")
                    .filter(|jobs| !jobs.is_empty())
                    .unwrap_or("4")
                    .into(),
            ),
        ];
        for name in ["USER", "TMPDIR"] {
            if let Some(value) = std::env::var_os(name) {
                env.push((name.to_string(), value));
            }
        }
        MacUpdate {
            checkout,
            cargo: cargo_home.join("bin").join("cargo"),
            env,
            log: state_dir.join("logs").join("update.log"),
            installed: installed.to_path_buf(),
            records: records.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            running: VERSION.into(),
            build_limit: BUILD_LIMIT,
            entitled,
            source: String::new(),
        }
        .checked()
    }

    /// The update from `self.checkout`, once it is known to be a git checkout; `None` when it is none.
    fn checked(self) -> Option<MacUpdate> {
        if !self.checkout.join(".git").exists() {
            return None;
        }
        let origin = git(&self.checkout, &["remote", "get-url", "origin"])
            .ok()
            .map(|url| redact_url(&url));
        Some(MacUpdate {
            source: format!(
                "{} {BRANCH}",
                origin.unwrap_or_else(|| self.checkout.display().to_string())
            ),
            ..self
        })
    }

    /// The program a build makes.
    fn built(&self) -> PathBuf {
        self.checkout.join("target/release/codefusion-runner")
    }

    /// The program kept beside the installed one.
    fn previous(&self) -> PathBuf {
        self.installed.with_extension(PREVIOUS)
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        git(&self.checkout, args)
    }

    /// Runs the build, its output into the log, for at most the build's limit.
    fn cargo_build(&self) -> Result<(), String> {
        if let Some(folder) = self.log.parent() {
            std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        }
        let log =
            std::fs::File::create(&self.log).map_err(|error| format!("{}: {error}", self.log.display()))?;
        let err = log
            .try_clone()
            .map_err(|error| format!("{}: {error}", self.log.display()))?;
        // Behind the jobs: they run on the same cores.
        let mut child = Command::new("/usr/bin/nice")
            .args(["-n", "10"])
            .arg(&self.cargo)
            .args(["xtask", "build"])
            .current_dir(&self.checkout)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err)
            .process_group(0)
            .spawn()
            .map_err(|error| format!("{}: {error}", self.cargo.display()))?;
        let status =
            wait_at_most(&mut child, self.build_limit).map_err(|error| format!("the build: {error}"))?;
        match status {
            Some(status) if status.success() => Ok(()),
            Some(status) => Err(format!(
                "the build failed ({status}): {}",
                last_error(&self.log).unwrap_or_else(|| format!("{} says why", self.log.display()))
            )),
            None => Err(format!(
                "the build took longer than {} and was ended",
                spoken(self.build_limit)
            )),
        }
    }
}

impl SelfUpdate for MacUpdate {
    fn source(&self) -> String {
        self.source.clone()
    }

    fn build(&self) -> Result<String, String> {
        let running = commit_of(&self.running)
            .filter(|commit| commit.len() == 12 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| {
                format!(
                    "{} names no commit it was built at; update it by hand",
                    self.running
                )
            })?;
        // What is checked out is the source of a build: changes of its own would go in unseen.
        let changed = self.git(&["status", "--porcelain", "--untracked-files=no"])?;
        if !changed.is_empty() {
            return Err(format!(
                "{} has changes of its own; nothing was built",
                self.checkout.display()
            ));
        }
        self.git(&["fetch", "-q", "origin", BRANCH])?;
        let target = self.git(&["rev-parse", "--verify", "-q", "FETCH_HEAD^{commit}"])?;
        let short = &target[..12.min(target.len())];
        if short == running {
            return Ok(self.running.clone());
        }
        // Main moves on; a Runner that is not on it is not taken back to what main says.
        if self
            .git(&["merge-base", "--is-ancestor", running, &target])
            .is_err()
        {
            return Err(format!(
                "{BRANCH} at {short} does not have {running}, which runs; update it by hand"
            ));
        }
        self.git(&["checkout", "-q", "--detach", &target])?;
        self.cargo_build()?;
        let built = self.built();
        let printed = run_at_most(Command::new(&built).arg("version"), GIT_LIMIT)
            .map_err(|error| format!("{} version: {error}", built.display()))?;
        let version = printed
            .trim()
            .strip_prefix("codefusion-runner ")
            .unwrap_or_default()
            .to_string();
        if commit_of(&version) != Some(short) {
            return Err(format!(
                "the program built says it is `{}`, not of {short}",
                cut(printed.trim(), 100)
            ));
        }
        (self.entitled)(&built)?;
        Ok(version)
    }

    fn install(&self, pending: &Pending) -> Result<(), String> {
        let records = standing(&self.records);
        if records > 0 {
            return Err(format!(
                "{records} {} a record; nothing was installed",
                if records == 1 {
                    "machine has"
                } else {
                    "machines have"
                }
            ));
        }
        // The one that runs, to go back to.
        copy_whole(&self.installed, &self.previous())?;
        let file = UpdateFile::in_state(&self.state_dir);
        file.write(pending)?;
        if let Err(error) = place(&self.built(), &self.installed, self.entitled) {
            let _ = file.remove();
            return Err(error);
        }
        Ok(())
    }
}

/// What the program does about an update under way before anything else at its start, in the state folder of the
/// installed program it runs as. `Err` ends it: the program before is back in place, and launchd starts that.
/// `Ok` is what the service says to the console once it runs.
pub fn guard(state_dir: &Path, installed: &Path, running: &str) -> Result<Started, String> {
    let file = UpdateFile::in_state(state_dir);
    let pending = match file.read() {
        Ok(pending) => pending,
        Err(error) => {
            // Nothing can be told of it: it goes, and the program runs.
            eprintln!("codefusion-runner: the update under way cannot be read, and is dropped: {error}");
            let _ = file.remove();
            None
        }
    };
    Ok(match at_start(pending, running) {
        AtStart::Nothing => Started::Nothing,
        AtStart::Trying(pending) => {
            file.write(&pending)?;
            Started::Trying(pending)
        }
        AtStart::GoBack(pending) => {
            let previous = installed.with_extension(PREVIOUS);
            if !previous.exists() {
                // Nothing to go back to: counting on would not change that.
                let _ = file.remove();
                return Err(format!(
                    "{} did not stay up, and there is no {} to go back to",
                    pending.to,
                    previous.display()
                ));
            }
            std::fs::rename(&previous, installed)
                .map_err(|error| format!("{}: {error}", installed.display()))?;
            file.write(&pending)?;
            return Err(format!(
                "{} did not stay up {} times; {} is back in place and launchd starts it",
                pending.to, pending.starts, pending.from
            ));
        }
        AtStart::Failed { id, message } => {
            file.remove()?;
            Started::Failed { id, message }
        }
    })
}

/// What [`guard`] found, for the service once it runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Started {
    /// No update was under way.
    Nothing,
    /// This is the new program: done once it stayed up.
    Trying(Pending),
    /// The update that was under way did not take.
    Failed {
        /// The console's command.
        id: String,
        /// What happened.
        message: String,
    },
}

/// The build's `PATH`: the toolchain, then the system's own tools (git, curl, tar, codesign).
fn build_path(cargo_home: &Path) -> OsString {
    let mut path = cargo_home.join("bin").into_os_string();
    path.push(":/usr/bin:/bin:/usr/sbin:/sbin");
    path
}

/// A remote's address without what may stand before an `@` of its host (a user or a token).
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let host_ends = rest.find('/').unwrap_or(rest.len());
    match rest[..host_ends].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// At most `chars` characters of `text`.
fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// The last line of the log that says `error`, cut to fit a result.
fn last_error(log: &Path) -> Option<String> {
    let file = std::fs::File::open(log).ok()?;
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| line.trim_start().starts_with("error"))
        .last()
        .map(|line| cut(line.trim(), 300))
}

/// Runs git in `checkout`, for at most [`GIT_LIMIT`]: its standard output, or what it said went wrong.
fn git(checkout: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("/usr/bin/git");
    command
        .arg("-C")
        .arg(checkout)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    run_at_most(&mut command, GIT_LIMIT).map_err(|error| format!("git {}: {error}", args.join(" ")))
}

/// Runs `command` for at most `limit`: its standard output when it succeeds, else what it said went wrong.
fn run_at_most(command: &mut Command, limit: Duration) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    // Read while it runs: a child that writes more than a pipe holds would wait for its reader.
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let status = wait_at_most(&mut child, limit).map_err(|error| error.to_string())?;
    let (stdout, stderr) = (
        stdout.join().unwrap_or_default(),
        stderr.join().unwrap_or_default(),
    );
    match status {
        Some(status) if status.success() => Ok(stdout.trim().to_string()),
        Some(status) if stderr.trim().is_empty() => Err(status.to_string()),
        Some(_) => Err(cut(stderr.trim(), 300)),
        None => Err(format!("took longer than {} and was ended", spoken(limit))),
    }
}

/// Waits for `child`, which leads a process group of its own, for at most `limit`; past it, ends the whole group
/// and answers `None`.
fn wait_at_most(child: &mut Child, limit: Duration) -> std::io::Result<Option<ExitStatus>> {
    let began = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if began.elapsed() >= limit {
            let group = libc::pid_t::try_from(child.id()).unwrap_or(libc::pid_t::MAX);
            // SAFETY: a signal to the process group the child leads (`process_group(0)`); no memory is shared.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
            child.wait()?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How many records are in `records`. Counted from the files, so that a look from beside a Runner that runs
/// changes nothing; a record nobody can read counts, since its machine may stand.
pub fn standing(records: &Path) -> usize {
    std::fs::read_dir(records)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .count()
}

/// Copies `from` to `to` in one step.
fn copy_whole(from: &Path, to: &Path) -> Result<(), String> {
    let fresh = to.with_extension("copy");
    std::fs::copy(from, &fresh).map_err(|error| format!("{}: {error}", fresh.display()))?;
    std::fs::rename(&fresh, to).map_err(|error| format!("{}: {error}", to.display()))
}

/// Puts a copy of `program` at `at` in one step, once the copy is signed for virtual machines.
pub fn place(program: &Path, at: &Path, entitled: fn(&Path) -> Result<(), String>) -> Result<(), String> {
    let folder = at.parent().ok_or("the program's folder has no parent")?;
    std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    let fresh = at.with_extension("new");
    std::fs::copy(program, &fresh).map_err(|error| format!("{}: {error}", fresh.display()))?;
    if let Err(error) = entitled(&fresh) {
        let _ = std::fs::remove_file(&fresh);
        return Err(error);
    }
    std::fs::rename(&fresh, at).map_err(|error| format!("{}: {error}", at.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed(_: &Path) -> Result<(), String> {
        Ok(())
    }

    fn unsigned(program: &Path) -> Result<(), String> {
        Err(format!("{} is not signed", program.display()))
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("/usr/bin/git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A remote with main at two commits, a checkout of it at the first, a toolchain whose `cargo` is `build`
    /// (a shell script run in the checkout), and a state folder with a program installed.
    struct Bench {
        _folder: tempfile::TempDir,
        root: PathBuf,
        first: String,
        second: String,
        update: MacUpdate,
    }

    /// A build that makes a program which says the commit it was built at, as `cargo xtask build` does.
    const BUILDS: &str = r#"echo built >> "$MARKS/builds"
mkdir -p target/release
printf '#!/bin/sh\necho "codefusion-runner 0.1.0+%s"\n' "$(/usr/bin/git rev-parse --short=12 HEAD)" > target/release/codefusion-runner
chmod +x target/release/codefusion-runner"#;

    fn bench(build: &str) -> Bench {
        let folder = tempfile::tempdir().expect("folder");
        let root = folder.path().canonicalize().expect("folder");
        let (origin, checkout) = (root.join("origin"), root.join("checkout"));
        std::fs::create_dir_all(&origin).expect("folder");
        run(&origin, &["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("Cargo.toml"), "one").expect("file");
        run(&origin, &["add", "."]);
        run(&origin, &["commit", "-q", "-m", "one"]);
        let first = run(&origin, &["rev-parse", "HEAD"]);
        run(
            &root,
            &["clone", "-q", origin.to_str().expect("path"), "checkout"],
        );
        std::fs::write(origin.join("Cargo.toml"), "two").expect("file");
        run(&origin, &["commit", "-q", "-am", "two"]);
        let second = run(&origin, &["rev-parse", "HEAD"]);
        std::fs::write(checkout.join(".git/info/exclude"), "target/\n").expect("file");

        let cargo_home = root.join("cargo");
        std::fs::create_dir_all(cargo_home.join("bin")).expect("folder");
        let cargo = cargo_home.join("bin/cargo");
        std::fs::write(&cargo, format!("#!/bin/sh\nset -e\n{build}\n")).expect("cargo");
        set_mode(&cargo, 0o755);
        let state = root.join("state");
        std::fs::create_dir_all(state.join("bin")).expect("folder");
        std::fs::create_dir_all(root.join("marks")).expect("folder");
        let installed = state.join("bin/codefusion-runner");
        std::fs::write(&installed, "the one that runs").expect("program");
        let update = MacUpdate {
            checkout,
            cargo,
            env: vec![
                ("PATH".into(), build_path(&cargo_home)),
                ("MARKS".into(), root.join("marks").into_os_string()),
            ],
            log: state.join("logs/update.log"),
            installed,
            records: state.join("run"),
            state_dir: state,
            running: format!("0.1.0+{}", &first[..12]),
            build_limit: Duration::from_secs(60),
            entitled: signed,
            source: String::new(),
        }
        .checked()
        .expect("a checkout");
        Bench {
            _folder: folder,
            root,
            first,
            second,
            update,
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
    }

    impl Bench {
        fn builds(&self) -> usize {
            std::fs::read_to_string(self.root.join("marks/builds"))
                .map(|text| text.lines().count())
                .unwrap_or(0)
        }

        fn head(&self) -> String {
            run(&self.update.checkout, &["rev-parse", "HEAD"])
        }

        fn pending(&self) -> Pending {
            Pending {
                id: "cmd-1".into(),
                from: self.update.running.clone(),
                to: format!("0.1.0+{}", &self.second[..12]),
                starts: 0,
            }
        }
    }

    #[test]
    fn main_ahead_is_fetched_checked_out_and_built_and_its_version_answered() {
        let bench = bench(BUILDS);
        assert_eq!(
            bench.update.source(),
            format!("{} main", bench.root.join("origin").display())
        );
        assert_eq!(bench.update.build(), Ok(format!("0.1.0+{}", &bench.second[..12])));
        assert_eq!(bench.head(), bench.second);
        assert_eq!(bench.builds(), 1);
    }

    #[test]
    fn main_at_the_running_commit_builds_nothing() {
        let mut bench = bench(BUILDS);
        bench.update.running = format!("0.1.0+{}", &bench.second[..12]);
        assert_eq!(bench.update.build(), Ok(bench.update.running.clone()));
        assert_eq!(bench.builds(), 0);
        assert_eq!(bench.head(), bench.first, "nothing is checked out");
    }

    #[test]
    fn a_checkout_with_changes_of_its_own_is_not_built() {
        let bench = bench(BUILDS);
        std::fs::write(bench.update.checkout.join("Cargo.toml"), "mine").expect("file");
        let said = bench.update.build().expect_err("refused");
        assert!(said.contains("has changes of its own"), "{said}");
        assert_eq!((bench.builds(), bench.head()), (0, bench.first.clone()));
    }

    #[test]
    fn a_runner_that_main_does_not_have_is_not_taken_back() {
        let mut bench = bench(BUILDS);
        let side = &bench.update.checkout;
        run(side, &["checkout", "-q", "-b", "side"]);
        std::fs::write(side.join("other"), "x").expect("file");
        run(side, &["add", "other"]);
        run(side, &["commit", "-q", "-m", "side"]);
        let running = run(side, &["rev-parse", "--short=12", "HEAD"]);
        bench.update.running = format!("0.1.0+{running}");
        let said = bench.update.build().expect_err("refused");
        assert_eq!(
            said,
            format!(
                "main at {} does not have {running}, which runs; update it by hand",
                &bench.second[..12]
            )
        );
        assert_eq!(bench.builds(), 0);
    }

    #[test]
    fn a_runner_that_names_no_commit_is_not_updated() {
        for running in ["0.1.0+unknown", "0.1.0", "0.1.0+446125c0a1b2-dirty", "0.1.0+"] {
            let mut bench = bench(BUILDS);
            bench.update.running = running.into();
            let said = bench.update.build().expect_err(running);
            assert!(said.contains("names no commit"), "{running}: {said}");
            assert_eq!(bench.builds(), 0);
        }
    }

    #[test]
    fn a_build_that_fails_says_its_last_error() {
        let bench = bench(
            "echo '   Compiling cf-daemon'\necho 'error[E0425]: cannot find value `x`' >&2\necho 'error: could not compile' >&2\nexit 101",
        );
        let said = bench.update.build().expect_err("failed");
        assert!(said.starts_with("the build failed (exit status: 101)"), "{said}");
        assert!(said.ends_with(": error: could not compile"), "{said}");
        let log = std::fs::read_to_string(&bench.update.log).expect("the log");
        assert!(log.contains("error[E0425]") && log.contains("Compiling"), "{log}");
    }

    #[test]
    fn a_build_past_its_limit_is_ended_with_what_it_started() {
        let mut bench = bench("sleep 30 &\necho $! > \"$MARKS/child\"\nwait");
        bench.update.build_limit = Duration::from_secs(1);
        let began = Instant::now();
        let said = bench.update.build().expect_err("ended");
        assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
        assert_eq!(said, "the build took longer than 1 second and was ended");
        let child = std::fs::read_to_string(bench.root.join("marks/child")).expect("the child");
        // SAFETY: signal 0 only asks whether the process is there.
        let alive = unsafe { libc::kill(child.trim().parse().expect("a pid"), 0) } == 0;
        assert!(!alive, "the build's own children end with it");
    }

    #[test]
    fn a_program_that_says_another_commit_or_is_not_signed_is_not_offered() {
        let bench = bench(
            "mkdir -p target/release\nprintf '#!/bin/sh\\necho codefusion-runner 0.1.0+aaaaaaaaaaaa-dirty\\n' > target/release/codefusion-runner\nchmod +x target/release/codefusion-runner",
        );
        let said = bench.update.build().expect_err("refused");
        assert!(said.contains("0.1.0+aaaaaaaaaaaa-dirty"), "{said}");

        let mut bench = self::bench(BUILDS);
        bench.update.entitled = unsigned;
        let said = bench.update.build().expect_err("refused");
        assert!(said.ends_with("is not signed"), "{said}");
    }

    #[test]
    fn an_install_keeps_the_running_program_writes_the_update_down_and_places_the_new_one() {
        let bench = bench(BUILDS);
        bench.update.build().expect("built");
        bench.update.install(&bench.pending()).expect("installed");
        let read = |path: &Path| std::fs::read_to_string(path).expect("a file");
        assert_eq!(read(&bench.update.previous()), "the one that runs");
        assert_eq!(read(&bench.update.installed), read(&bench.update.built()));
        assert_eq!(
            UpdateFile::in_state(&bench.update.state_dir).read(),
            Ok(Some(bench.pending()))
        );
        let left: Vec<_> = std::fs::read_dir(bench.update.installed.parent().expect("bin"))
            .expect("bin")
            .flatten()
            .map(|entry| entry.file_name().into_string().expect("a name"))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(left, ["codefusion-runner", "codefusion-runner.prev"]);
    }

    #[test]
    fn an_install_is_refused_while_a_machine_has_a_record_and_changes_nothing() {
        let bench = bench(BUILDS);
        bench.update.build().expect("built");
        std::fs::create_dir_all(&bench.update.records).expect("folder");
        std::fs::write(bench.update.records.join("job.json"), "{}").expect("record");
        assert_eq!(
            bench.update.install(&bench.pending()),
            Err("1 machine has a record; nothing was installed".into())
        );
        assert_eq!(
            std::fs::read_to_string(&bench.update.installed).expect("program"),
            "the one that runs"
        );
        assert!(!bench.update.previous().exists());
        assert_eq!(UpdateFile::in_state(&bench.update.state_dir).read(), Ok(None));
    }

    #[test]
    fn an_install_whose_program_cannot_be_placed_leaves_no_update_written_down() {
        let mut bench = bench(BUILDS);
        bench.update.build().expect("built");
        bench.update.entitled = unsigned;
        assert!(bench.update.install(&bench.pending()).is_err());
        assert_eq!(
            std::fs::read_to_string(&bench.update.installed).expect("program"),
            "the one that runs"
        );
        assert_eq!(UpdateFile::in_state(&bench.update.state_dir).read(), Ok(None));
    }

    /// The starts of an installed program, as launchd makes them: the new one counts its starts, goes back after
    /// the last, and the one before says it failed.
    #[test]
    fn a_new_program_that_does_not_stay_up_puts_the_one_before_back_which_says_so() {
        let folder = tempfile::tempdir().expect("folder");
        let state = folder.path();
        let installed = state.join("bin/codefusion-runner");
        std::fs::create_dir_all(installed.parent().expect("bin")).expect("folder");
        std::fs::write(&installed, "new").expect("program");
        std::fs::write(installed.with_extension(PREVIOUS), "old").expect("program");
        let pending = Pending {
            id: "cmd-1".into(),
            from: "0.1.0+aaaaaaaaaaaa".into(),
            to: "0.1.0+bbbbbbbbbbbb".into(),
            starts: 0,
        };
        let file = UpdateFile::in_state(state);
        assert_eq!(guard(state, &installed, &pending.to), Ok(Started::Nothing));
        file.write(&pending).expect("written");
        for starts in 1..=cf_daemon::update::MAX_STARTS {
            let started = guard(state, &installed, &pending.to);
            let trying = Pending {
                starts,
                ..pending.clone()
            };
            assert_eq!(started, Ok(Started::Trying(trying.clone())));
            assert_eq!(file.read(), Ok(Some(trying)), "each start is counted on disk");
        }
        let said = guard(state, &installed, &pending.to).expect_err("goes back");
        assert!(said.contains("is back in place"), "{said}");
        assert_eq!(std::fs::read_to_string(&installed).expect("program"), "old");
        assert!(!installed.with_extension(PREVIOUS).exists());
        assert_eq!(
            guard(state, &installed, &pending.from),
            Ok(Started::Failed {
                id: "cmd-1".into(),
                message:
                    "0.1.0+bbbbbbbbbbbb did not stay up and was taken back: 0.1.0+aaaaaaaaaaaa runs again \
                          (logs/launchd.log says why)"
                        .into(),
            })
        );
        assert_eq!(file.read(), Ok(None), "said once");
        assert_eq!(guard(state, &installed, &pending.from), Ok(Started::Nothing));
    }

    #[test]
    fn a_new_program_with_nothing_to_go_back_to_stops_counting() {
        let folder = tempfile::tempdir().expect("folder");
        let installed = folder.path().join("bin/codefusion-runner");
        let file = UpdateFile::in_state(folder.path());
        file.write(&Pending {
            id: "cmd-1".into(),
            from: "0.1.0+aaaaaaaaaaaa".into(),
            to: "0.1.0+bbbbbbbbbbbb".into(),
            starts: cf_daemon::update::MAX_STARTS,
        })
        .expect("written");
        let said = guard(folder.path(), &installed, "0.1.0+bbbbbbbbbbbb").expect_err("no way back");
        assert!(said.contains("no ") && said.contains("to go back to"), "{said}");
        assert_eq!(file.read(), Ok(None));
        assert_eq!(
            guard(folder.path(), &installed, "0.1.0+bbbbbbbbbbbb"),
            Ok(Started::Nothing)
        );
    }

    #[test]
    fn an_update_file_nobody_can_read_is_dropped_and_the_program_runs() {
        let folder = tempfile::tempdir().expect("folder");
        let installed = folder.path().join("bin/codefusion-runner");
        std::fs::write(folder.path().join(cf_daemon::update::UPDATE_FILE), "{half").expect("file");
        assert_eq!(
            guard(folder.path(), &installed, "0.1.0+bbbbbbbbbbbb"),
            Ok(Started::Nothing)
        );
        assert!(!folder.path().join(cf_daemon::update::UPDATE_FILE).exists());
    }

    #[test]
    fn a_remote_is_named_without_who_reaches_it() {
        for (url, said) in [
            (
                "https://x-access-token:ghs_abc@github.com/o/r.git",
                "https://github.com/o/r.git",
            ),
            ("https://user@github.com/o/r", "https://github.com/o/r"),
            ("https://github.com/o/r", "https://github.com/o/r"),
            ("https://github.com/o/r@v1", "https://github.com/o/r@v1"),
            ("git@github.com:o/r.git", "git@github.com:o/r.git"),
            ("/Volumes/x/origin", "/Volumes/x/origin"),
            ("", ""),
        ] {
            assert_eq!(redact_url(url), said, "{url}");
        }
    }
}
