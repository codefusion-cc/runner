//! The Runner's systemd entry on Linux: a unit of the user's own systemd, `codefusion-runner.service`.
//!
//! A user unit and not one of the system, as on a Mac: it is installed and removed without root, writes nothing
//! outside the user's home and the state folder, and the GitHub App's key and the jobs' disks stay that user's files.
//! The user is one of its own (`codefusion-runner`) with lingering on, so its systemd runs from the host's start and
//! not only while someone is logged in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::manager::ServiceManager;

/// The unit's name.
pub const UNIT: &str = "codefusion-runner.service";

/// The unit's text.
///
/// `Restart=always` starts the Runner again whenever it ends, as launchd's `KeepAlive` does: `restart` and an update
/// work by ending, and machines that stand are found again by the Runner that starts. systemd waits `RestartSec`
/// after every end, a restart's too, so it is short; a Runner that cannot start (no image, no key) waits out the rest
/// of [`crate::service::FAILED_START_WAIT`] itself before it ends, which keeps it to two tries and two lines in the log
/// a minute as launchd's `ThrottleInterval` does, and `StartLimitIntervalSec=0` lets it try for as long as it takes. `Type=exec` makes `systemctl start` fail when the program cannot be run at all.
/// `TimeoutStopSec` gives it the time to close its sessions at GitHub. Each vm-host runs in a scope of its own, so
/// `KillMode=mixed` ends only what the Runner itself left in the unit once it is gone, never a machine. The output
/// goes to a file of the state folder, beside the Runner's own log, as on a Mac. Nothing in it confines the Runner:
/// its vm-hosts make namespaces, cgroups and taps, and open `/dev/kvm`.
pub fn unit(program: &Path, config: &Path, state_dir: &Path, log: &Path) -> Result<String, String> {
    Ok(format!(
        "# Written by `codefusion-runner install`; `codefusion-runner uninstall` removes it.
[Unit]
Description=CodeFusion Runner
Documentation=https://github.com/codefusion-cc/runner
StartLimitIntervalSec=0

[Service]
Type=exec
ExecStart={program} daemon --config {config}
WorkingDirectory={state}
Restart=always
RestartSec=5
TimeoutStopSec=90
KillMode=mixed
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=default.target
",
        program = word(program)?,
        config = word(config)?,
        state = setting(state_dir)?,
        log = setting(log)?,
    ))
}

/// A path that systemd reads back as written, or why it cannot: a control character ends a line or is no part of
/// a path systemd takes, and white space or a backslash at its end would be cut off or join the next line.
fn plain(path: &Path) -> Result<&str, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{}: a unit can only name a path in UTF-8", path.display()))?;
    if text.chars().any(char::is_control) || text.ends_with(char::is_whitespace) || text.ends_with('\\') {
        return Err(format!(
            "{text:?}: a unit cannot name a path with a control character, or one that ends in white space or a \
             backslash"
        ));
    }
    Ok(text)
}

/// A path as the value of a setting such as `WorkingDirectory=`: taken whole, with `%` its only special character.
fn setting(path: &Path) -> Result<String, String> {
    Ok(plain(path)?.replace('%', "%%"))
}

/// A path as one word of a command line: in double quotes, where a backslash and a quote are escaped, and `%` and
/// `$` are doubled, since systemd reads them as specifiers and variables there too.
fn word(path: &Path) -> Result<String, String> {
    let mut quoted = String::from('"');
    for char in plain(path)?.chars() {
        match char {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

/// How long the Runner waits between two looks at what systemd has.
pub const UNIT_PAUSE: Duration = Duration::from_millis(200);

/// How often [`UserUnit::stop`] looks whether the unit is down: with [`UNIT_PAUSE`] between two looks, ten seconds
/// more than the unit's `TimeoutStopSec`.
const STOP_TRIES: u32 = 500;

/// Runs `systemctl --user` with these arguments and says what it printed, as an error when it failed.
pub type Systemctl<'a> = &'a dyn Fn(&[&str]) -> Result<String, String>;

/// The real `systemctl --user`. The user's systemd is found through its runtime folder, which a shell of
/// `sudo -iu <user>` may not name ([`cf_daemon::platform::user_bus_env`]).
pub fn systemctl(args: &[&str]) -> Result<String, String> {
    let env = cf_daemon::platform::this_user_bus_env()?;
    let output = std::process::Command::new("/usr/bin/systemctl")
        .arg("--user")
        .args(args)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("systemctl: {error}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!("systemctl --user {}: {}", args.join(" "), text.trim()))
    }
}

/// The configuration folder the user's systemd reads units from: `XDG_CONFIG_HOME` as that systemd has it, which is
/// not always what the shell that installs has, else `~/.config`.
pub fn config_home(home: &Path, run: Systemctl<'_>) -> PathBuf {
    run(&["show-environment"])
        .ok()
        .and_then(|env| {
            env.lines()
                .find_map(|line| line.strip_prefix("XDG_CONFIG_HOME="))
                .map(PathBuf::from)
        })
        .filter(|folder| folder.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
}

/// The Runner's unit in one user's systemd.
pub struct UserUnit<'a> {
    /// The user's configuration folder: `XDG_CONFIG_HOME`, else `~/.config`.
    pub config_home: PathBuf,
    /// `systemctl --user`.
    pub run: Systemctl<'a>,
    /// How long to wait between two looks.
    pub pause: Duration,
}

impl ServiceManager for UserUnit<'_> {
    fn name(&self) -> String {
        format!("the systemd user unit {UNIT}")
    }

    /// Where the unit file is.
    fn path(&self) -> PathBuf {
        self.config_home.join("systemd/user").join(UNIT)
    }

    fn definition(
        &self,
        program: &Path,
        config: &Path,
        state_dir: &Path,
        log: &Path,
    ) -> Result<String, String> {
        unit(program, config, state_dir, log)
    }

    fn look(&self) -> String {
        "systemctl --user status codefusion-runner".into()
    }

    /// Ends the unit if it runs, also between two starts, and waits until systemd has it down. The Runner is sent a
    /// SIGTERM and has the unit's `TimeoutStopSec` to end, so the wait is longer than that. What `stop` says
    /// decides nothing on its own: whether the unit is down does.
    fn stop(&self) -> Result<(), String> {
        if self.down()? {
            return Ok(());
        }
        let said = (self.run)(&["stop", UNIT]).err();
        for _ in 0..STOP_TRIES {
            if self.down()? {
                return Ok(());
            }
            std::thread::sleep(self.pause);
        }
        Err(match said {
            Some(said) => format!("systemd did not stop {UNIT} ({said})"),
            None => format!("systemd did not stop {UNIT}"),
        })
    }

    /// Writes the unit and has systemd start it, and at every start of the user's systemd from now on, in place of
    /// one that ran before.
    fn start(&self, unit: &str) -> Result<(), String> {
        self.stop()?;
        let path = self.path();
        let folder = path.parent().ok_or("the unit's folder has no parent")?;
        std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        cf_secret::replace_private(&path, unit.as_bytes())
            .map_err(|error| format!("{}: {error}", path.display()))?;
        (self.run)(&["daemon-reload"])?;
        (self.run)(&["enable", UNIT])?;
        (self.run)(&["start", UNIT])?;
        Ok(())
    }

    /// Ends the unit and removes it. Nothing there is nothing to do.
    fn remove(&self) -> Result<(), String> {
        self.stop()?;
        let path = self.path();
        if path.exists() {
            (self.run)(&["disable", UNIT])?;
            std::fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            (self.run)(&["daemon-reload"])?;
        }
        // A unit that failed is remembered as failed until it is reset; one that is gone has nothing to reset.
        let _ = (self.run)(&["reset-failed", UNIT]);
        Ok(())
    }
}

impl UserUnit<'_> {
    /// The unit's state as systemd says it: `active`, `activating`, `deactivating`, `inactive`, `failed`.
    fn state(&self) -> Result<String, String> {
        (self.run)(&["show", "--property=ActiveState", "--value", UNIT]).map(|text| text.trim().to_string())
    }

    /// Whether the unit is down: nothing of it runs, and systemd starts nothing of it by itself.
    fn down(&self) -> Result<bool, String> {
        Ok(matches!(self.state()?.as_str(), "inactive" | "failed" | ""))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    fn value<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
        text.lines()
            .filter_map(|line| line.strip_prefix(&format!("{key}=")))
            .collect()
    }

    #[test]
    fn the_unit_runs_the_daemon_restarts_it_forever_and_leaves_its_vm_hosts_alone() {
        let text = unit(
            Path::new("/home/codefusion-runner/.local/state/codefusion-runner/bin/codefusion-runner"),
            Path::new("/home/codefusion-runner/.config/codefusion-runner/config.json"),
            Path::new("/home/codefusion-runner/.local/state/codefusion-runner"),
            Path::new("/home/codefusion-runner/.local/state/codefusion-runner/logs/systemd.log"),
        )
        .expect("a unit");
        assert_eq!(
            value(&text, "ExecStart"),
            [
                "\"/home/codefusion-runner/.local/state/codefusion-runner/bin/codefusion-runner\" daemon --config \
                 \"/home/codefusion-runner/.config/codefusion-runner/config.json\""
            ]
        );
        assert_eq!(
            value(&text, "WorkingDirectory"),
            ["/home/codefusion-runner/.local/state/codefusion-runner"]
        );
        assert_eq!(value(&text, "Type"), ["exec"]);
        assert_eq!(value(&text, "Restart"), ["always"]);
        // Short, so that a restart or an update is back in seconds: one that cannot start waits on its own.
        assert_eq!(value(&text, "RestartSec"), ["5"]);
        assert_eq!(value(&text, "StartLimitIntervalSec"), ["0"]);
        assert_eq!(value(&text, "TimeoutStopSec"), ["90"]);
        // The machines' vm-hosts are in scopes of their own: a stop ends the Runner with SIGTERM, then only what
        // it left in its own cgroup.
        assert_eq!(value(&text, "KillMode"), ["mixed"]);
        let log = "append:/home/codefusion-runner/.local/state/codefusion-runner/logs/systemd.log";
        assert_eq!(value(&text, "StandardOutput"), [log]);
        assert_eq!(value(&text, "StandardError"), [log]);
        assert_eq!(value(&text, "WantedBy"), ["default.target"]);
        // Nothing that confines the Runner or what it starts: its vm-hosts make namespaces, cgroups and taps.
        for key in [
            "Delegate",
            "PrivateTmp",
            "PrivateDevices",
            "ProtectSystem",
            "ProtectHome",
            "NoNewPrivileges",
            "RestrictNamespaces",
            "DevicePolicy",
            "SystemCallFilter",
            "MemoryMax",
            "Nice",
            "CPUQuota",
        ] {
            assert!(value(&text, key).is_empty(), "{key}");
        }
    }

    #[test]
    fn paths_with_spaces_quotes_and_systemds_own_characters_stay_one_path() {
        let text = unit(
            Path::new("/srv/Code Fusion/50% \"x\" $HOME \\d/bin/codefusion-runner"),
            Path::new("/etc/cf;runner/con'fig.json"),
            Path::new("/srv/Code Fusion/50% $HOME"),
            Path::new("/srv/Code Fusion/50% $HOME/logs/systemd.log"),
        )
        .expect("a unit");
        assert_eq!(
            value(&text, "ExecStart"),
            [
                "\"/srv/Code Fusion/50%% \\\"x\\\" $$HOME \\\\d/bin/codefusion-runner\" daemon --config \
                 \"/etc/cf;runner/con'fig.json\""
            ]
        );
        // A setting's value is taken whole; only its specifiers are read.
        assert_eq!(value(&text, "WorkingDirectory"), ["/srv/Code Fusion/50%% $HOME"]);
        assert_eq!(
            value(&text, "StandardOutput"),
            ["append:/srv/Code Fusion/50%% $HOME/logs/systemd.log"]
        );
    }

    #[test]
    fn a_path_systemd_would_not_read_back_as_written_is_refused() {
        let fine = Path::new("/state");
        for bad in ["/a\nExecStartPre=/bin/evil", "/a\tb", "/a ", "/a\\", "/a\r"] {
            let error = unit(Path::new(bad), fine, fine, fine).expect_err(bad);
            assert!(error.contains("cannot name a path"), "{bad:?}: {error}");
            assert!(unit(fine, fine, Path::new(bad), fine).is_err(), "{bad:?}");
            assert!(unit(fine, fine, fine, Path::new(bad)).is_err(), "{bad:?}");
            assert!(unit(fine, Path::new(bad), fine, fine).is_err(), "{bad:?}");
        }
        assert!(unit(Path::new("/ä/ü b"), fine, fine, fine).is_ok());
    }

    /// A user's systemd that has the unit in some state, and writes down what it was asked.
    struct Fake {
        /// What each `show` answers in turn; the last one stays.
        states: RefCell<VecDeque<&'static str>>,
        calls: RefCell<Vec<String>>,
        /// What `stop` answers, and the states it leaves behind.
        stop: RefCell<(Result<(), &'static str>, Vec<&'static str>)>,
        /// Calls that fail, with what systemctl says.
        fails: RefCell<Vec<(&'static str, &'static str)>>,
    }

    impl Fake {
        fn new(state: &'static str) -> Fake {
            Fake {
                states: RefCell::new(VecDeque::from([state])),
                calls: RefCell::default(),
                stop: RefCell::new((Ok(()), vec!["inactive"])),
                fails: RefCell::default(),
            }
        }

        fn run(&self, args: &[&str]) -> Result<String, String> {
            let call = args.join(" ");
            self.calls.borrow_mut().push(call.clone());
            if let Some((_, says)) = self.fails.borrow().iter().find(|(name, _)| *name == args[0]) {
                return Err(format!("systemctl --user {call}: {says}"));
            }
            match args[0] {
                "show" => {
                    let mut states = self.states.borrow_mut();
                    let now = if states.len() > 1 {
                        states.pop_front()
                    } else {
                        states.front().copied()
                    };
                    Ok(format!("{}\n", now.unwrap_or("inactive")))
                }
                "stop" => {
                    let (answer, after) = self.stop.borrow().clone();
                    *self.states.borrow_mut() = after.into_iter().collect();
                    answer
                        .map(|()| String::new())
                        .map_err(|says| format!("systemctl --user {call}: {says}"))
                }
                "start" => {
                    *self.states.borrow_mut() = VecDeque::from(["active"]);
                    Ok(String::new())
                }
                "daemon-reload" | "enable" | "disable" | "reset-failed" => Ok(String::new()),
                other => Err(format!("unexpected {other}")),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    fn unit_of<'a>(home: &Path, run: Systemctl<'a>) -> UserUnit<'a> {
        UserUnit {
            config_home: home.join(".config"),
            run,
            pause: Duration::ZERO,
        }
    }

    const SHOW: &str = "show --property=ActiveState --value codefusion-runner.service";

    #[test]
    fn the_unit_goes_where_the_users_systemd_reads_units_whatever_the_shell_says() {
        let home = Path::new("/home/codefusion-runner");
        let manager_with = |env: &'static str| {
            move |args: &[&str]| -> Result<String, String> {
                assert_eq!(args, ["show-environment"]);
                Ok(env.to_string())
            }
        };
        let own = manager_with(
            "HOME=/home/codefusion-runner\nXDG_CONFIG_HOME=/srv/cf/config\nXDG_RUNTIME_DIR=/run/user/1002\n",
        );
        assert_eq!(config_home(home, &own), Path::new("/srv/cf/config"));
        let none = manager_with("HOME=/home/codefusion-runner\nXDG_RUNTIME_DIR=/run/user/1002\n");
        assert_eq!(config_home(home, &none), home.join(".config"));
        // A value systemd had to quote, or a relative one, is not a folder it reads.
        let quoted = manager_with("XDG_CONFIG_HOME=$'/srv/a\\nb'\n");
        assert_eq!(config_home(home, &quoted), home.join(".config"));
        let unreachable = |_: &[&str]| -> Result<String, String> { Err("Failed to connect to bus".into()) };
        assert_eq!(config_home(home, &unreachable), home.join(".config"));
    }

    #[test]
    fn a_first_start_writes_the_unit_reloads_enables_and_starts_it() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new("inactive");
        let run = |args: &[&str]| fake.run(args);
        let unit = unit_of(home.path(), &run);
        unit.start("[Service]\n").expect("started");
        let path = home.path().join(".config/systemd/user/codefusion-runner.service");
        assert_eq!(unit.path(), path);
        assert_eq!(std::fs::read_to_string(&path).expect("unit"), "[Service]\n");
        assert_eq!(
            fake.calls(),
            [
                SHOW,
                "daemon-reload",
                "enable codefusion-runner.service",
                "start codefusion-runner.service"
            ]
        );
    }

    #[test]
    fn a_start_over_a_running_unit_stops_it_first_and_waits_until_it_is_down() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new("active");
        // systemctl's stop returns, and the Runner still closes its sessions for a while.
        fake.stop.borrow_mut().1 = vec!["deactivating", "deactivating", "deactivating", "inactive"];
        let run = |args: &[&str]| fake.run(args);
        unit_of(home.path(), &run).start("[Service]\n").expect("started");
        let calls = fake.calls();
        assert_eq!(calls[0], SHOW);
        assert_eq!(calls[1], "stop codefusion-runner.service");
        let first_write = calls
            .iter()
            .position(|call| call == "daemon-reload")
            .expect("a reload");
        assert_eq!(
            calls[2..first_write].iter().filter(|call| *call == SHOW).count(),
            4,
            "it looked until the unit was down, and only then wrote and reloaded: {calls:?}"
        );
        assert_eq!(
            calls.last().map(String::as_str),
            Some("start codefusion-runner.service")
        );
    }

    #[test]
    fn a_unit_between_two_starts_is_stopped_too() {
        let home = tempfile::tempdir().expect("home");
        // `Restart=always` waits `RestartSec` in `activating`: a stop then keeps it from starting again.
        let fake = Fake::new("activating");
        let run = |args: &[&str]| fake.run(args);
        unit_of(home.path(), &run).stop().expect("stopped");
        assert_eq!(fake.calls(), [SHOW, "stop codefusion-runner.service", SHOW]);
    }

    #[test]
    fn a_stop_that_never_completes_is_an_error_with_systemctls_words() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new("active");
        *fake.stop.borrow_mut() = (
            Err("Job for codefusion-runner.service canceled."),
            vec!["deactivating"],
        );
        let run = |args: &[&str]| fake.run(args);
        let error = unit_of(home.path(), &run).stop().expect_err("never down");
        assert_eq!(
            error,
            "systemd did not stop codefusion-runner.service (systemctl --user stop codefusion-runner.service: Job \
             for codefusion-runner.service canceled.)"
        );
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| call.starts_with("stop"))
                .count(),
            1,
            "asked once, then waited"
        );
        assert_eq!(
            fake.calls().iter().filter(|call| *call == SHOW).count(),
            1 + STOP_TRIES as usize,
            "waited the whole time"
        );
    }

    #[test]
    fn a_stop_that_says_it_failed_while_the_unit_went_down_is_done() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new("active");
        *fake.stop.borrow_mut() = (
            Err("Job for codefusion-runner.service failed."),
            vec!["deactivating", "failed"],
        );
        let run = |args: &[&str]| fake.run(args);
        unit_of(home.path(), &run).stop().expect("down");
    }

    #[test]
    fn a_systemd_that_cannot_be_asked_is_an_error_and_nothing_is_written() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new("inactive");
        fake.fails
            .borrow_mut()
            .push(("show", "Failed to connect to bus: No medium found"));
        let run = |args: &[&str]| fake.run(args);
        let unit = unit_of(home.path(), &run);
        let error = unit.start("[Service]\n").expect_err("no systemd");
        assert!(
            error.ends_with("Failed to connect to bus: No medium found"),
            "{error}"
        );
        assert!(!unit.path().exists());
    }

    #[test]
    fn a_unit_that_does_not_start_is_an_error_with_systemctls_words_and_stays_installed_to_be_looked_at() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new("inactive");
        fake.fails.borrow_mut().push((
            "start",
            "Job for codefusion-runner.service failed because the control process exited with error code.",
        ));
        let run = |args: &[&str]| fake.run(args);
        let unit = unit_of(home.path(), &run);
        let error = unit.start("[Service]\n").expect_err("not started");
        assert!(
            error.contains("control process exited with error code"),
            "{error}"
        );
        assert!(unit.path().exists());
    }

    #[test]
    fn removing_stops_disables_and_removes_the_unit_and_nothing_there_is_no_error() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new("inactive");
        let run = |args: &[&str]| fake.run(args);
        let unit = unit_of(home.path(), &run);
        unit.remove().expect("nothing to remove");
        assert_eq!(fake.calls(), [SHOW, "reset-failed codefusion-runner.service"]);
        unit.start("[Service]\n").expect("started");
        fake.calls.borrow_mut().clear();
        unit.remove().expect("removed");
        assert!(!unit.path().exists());
        assert_eq!(
            fake.calls(),
            [
                SHOW,
                "stop codefusion-runner.service",
                SHOW,
                "disable codefusion-runner.service",
                "daemon-reload",
                "reset-failed codefusion-runner.service"
            ]
        );
    }

    /// The unit as `install` writes it, with this user's real systemd and the real daemon, which ends at once (no
    /// image is installed, and it waits out `FAILED_START_WAIT` first): systemd starts it again, and `remove` leaves
    /// nothing behind. It
    /// installs `codefusion-runner.service` for this user, so it runs only when asked:
    /// `cargo test -p codefusion-runner -- --ignored the_real_systemd`.
    #[test]
    #[ignore = "installs a unit in this user's systemd"]
    fn the_real_systemd_starts_the_daemon_again_after_it_ends_and_takes_the_unit_back_without_a_trace() {
        use std::os::unix::fs::PermissionsExt as _;
        let show = |property: &str| {
            systemctl(&["show", &format!("--property={property}"), "--value", UNIT])
                .expect("systemctl")
                .trim()
                .to_string()
        };
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
        let manager = UserUnit {
            config_home: config_home(&home, &systemctl),
            run: &systemctl,
            pause: UNIT_PAUSE,
        };
        assert!(
            !manager.path().exists(),
            "{} is there already, and is left alone",
            manager.path().display()
        );
        // Taken back however the test ends.
        struct Back<'a>(&'a UserUnit<'a>);
        impl Drop for Back<'_> {
            fn drop(&mut self) {
                let _ = self.0.remove();
            }
        }
        let _back = Back(&manager);

        let program = std::env::current_exe()
            .expect("this test")
            .parent()
            .and_then(Path::parent)
            .expect("the target folder")
            .join("codefusion-runner");
        let folder = tempfile::tempdir().expect("folder");
        let (state, config) = (folder.path().join("state 1%"), folder.path().join("config.json"));
        let log = state.join("logs").join("systemd.log");
        std::fs::create_dir_all(log.parent().expect("logs")).expect("folder");
        std::fs::write(
            &config,
            serde_json::json!({
                "machine": "unit-trial",
                "state_dir": state,
                "app_dir": folder.path().join("app"),
                "image": "0.0.0",
            })
            .to_string(),
        )
        .expect("config");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).expect("a mode");

        manager
            .start(&unit(&program, &config, &state, &log).expect("a unit"))
            .expect("started");
        assert_eq!(show("UnitFileState"), "enabled");
        let refusals = || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .matches("image 0.0.0 is not installed")
                .count()
        };
        let began = std::time::Instant::now();
        while refusals() < 2 || show("NRestarts").parse::<u32>().unwrap_or(0) < 1 {
            assert!(
                began.elapsed() < Duration::from_secs(60),
                "systemd did not start the daemon again: {} lines, {} restarts, {}",
                refusals(),
                show("NRestarts"),
                show("ActiveState")
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        // Between two starts it waits `RestartSec`: a stop then keeps it down.
        manager.remove().expect("removed");
        assert!(!manager.path().exists());
        assert_eq!(show("LoadState"), "not-found");
        assert_eq!(show("MainPID"), "0");
    }
}
