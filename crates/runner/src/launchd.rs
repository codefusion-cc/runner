//! The Runner's launchd entry: an agent of the person's own session.
//!
//! An agent and not a daemon of the system: it is installed and removed without a password and writes nothing
//! outside the person's home, the Mac's GitHub App key and the jobs' disks stay that person's files, and both
//! Macs have a session whenever they are on (the cf-runners manager is an agent for the same reasons). The price
//! is that no job runs between a start of the Mac and the log-in; a Mac without a person logs in by itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The agent's name. The cf-runners manager's is `cc.codefusion.runners`; the two never share one.
pub const LABEL: &str = "cc.codefusion.vm-runner";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The agent's property list.
///
/// `ProcessType` is `Interactive`: launchd throttles the CPU and the disk of a job that names no type, and of
/// every process it starts, and a vm-host that is throttled boots no machine in time (one under the background
/// policy got no CPU for 150 seconds). `KeepAlive` starts the Runner again when it ends, whatever the reason; the
/// machines that stand are found again by the one that starts. `ThrottleInterval` keeps a Runner that cannot
/// start (no image, no key) to two tries and two log lines a minute; one that ran longer starts again at once.
/// `ExitTimeOut` gives it the time to close its sessions at GitHub before launchd ends it for good.
pub fn plist(program: &Path, config: &Path, state_dir: &Path) -> String {
    let log = state_dir.join("logs").join("launchd.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
		<string>daemon</string>
		<string>--config</string>
		<string>{config}</string>
	</array>
	<key>WorkingDirectory</key>
	<string>{state}</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>30</integer>
	<key>ExitTimeOut</key>
	<integer>60</integer>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        program = escape(&program.to_string_lossy()),
        config = escape(&config.to_string_lossy()),
        state = escape(&state_dir.to_string_lossy()),
        log = escape(&log.to_string_lossy()),
    )
}

/// Runs `launchctl` with these arguments and says what it printed, as an error when it failed.
pub type Launchctl<'a> = &'a dyn Fn(&[&str]) -> Result<String, String>;

/// The real `launchctl`.
pub fn launchctl(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("/bin/launchctl")
        .args(args)
        .output()
        .map_err(|error| format!("launchctl: {error}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!("launchctl {}: {}", args.join(" "), text.trim()))
    }
}

/// The agent of one person on this Mac.
pub struct Agent<'a> {
    /// The person's home.
    pub home: &'a Path,
    /// The person's number, which names the session's domain.
    pub uid: u32,
    /// `launchctl`.
    pub run: Launchctl<'a>,
    /// How long to wait between two tries while launchd still unloads the agent of before.
    pub pause: Duration,
}

impl Agent<'_> {
    /// Where the property list is.
    pub fn path(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self) -> String {
        format!("{}/{LABEL}", self.domain())
    }

    /// Whether launchd has the agent.
    pub fn loaded(&self) -> bool {
        (self.run)(&["print", &self.target()]).is_ok()
    }

    /// Ends the agent if launchd has it and waits until launchd has let go of it. The Runner is sent a SIGTERM
    /// and has the property list's `ExitTimeOut` to end.
    pub fn stop(&self) -> Result<(), String> {
        if !self.loaded() {
            return Ok(());
        }
        (self.run)(&["bootout", &self.target()])?;
        for _ in 0..150 {
            if !self.loaded() {
                return Ok(());
            }
            std::thread::sleep(self.pause);
        }
        Err("launchd did not let go of the agent".into())
    }

    /// Writes the property list and has launchd start the agent, in place of one that ran before.
    pub fn start(&self, plist: &str) -> Result<(), String> {
        self.stop()?;
        let path = self.path();
        let folder = path.parent().ok_or("the agent's folder has no parent")?;
        std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        std::fs::write(&path, plist).map_err(|error| format!("{}: {error}", path.display()))?;
        let path = path.to_string_lossy().into_owned();
        // launchd may still be busy with the agent it just let go of: the first tries can fail.
        let mut last = String::new();
        for _ in 0..10 {
            match (self.run)(&["bootstrap", &self.domain(), &path]) {
                Ok(_) => return Ok(()),
                Err(error) => last = error,
            }
            std::thread::sleep(self.pause);
        }
        Err(last)
    }

    /// Ends the agent and removes its property list. Nothing there is nothing to do.
    pub fn remove(&self) -> Result<(), String> {
        self.stop()?;
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("{}: {error}", self.path().display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn the_property_list_runs_the_daemon_unthrottled_and_keeps_it_alive() {
        let text = plist(
            Path::new("/Users/someone/Library/Application Support/CodeFusion Runner/bin/codefusion-runner"),
            Path::new("/Users/someone/.config/codefusion-runner/config.json"),
            Path::new("/Users/someone/Library/Application Support/CodeFusion Runner"),
        );
        let after = |key: &str| {
            let at = text
                .find(&format!("<key>{key}</key>"))
                .unwrap_or_else(|| panic!("no {key}"));
            text[at..].lines().nth(1).expect("a value").trim().to_string()
        };
        assert_eq!(after("Label"), "<string>cc.codefusion.vm-runner</string>");
        assert_eq!(after("ProcessType"), "<string>Interactive</string>");
        assert_eq!(after("KeepAlive"), "<true/>");
        assert_eq!(after("RunAtLoad"), "<true/>");
        assert_eq!(after("ExitTimeOut"), "<integer>60</integer>");
        assert_eq!(
            after("StandardErrorPath"),
            "<string>/Users/someone/Library/Application Support/CodeFusion Runner/logs/launchd.log</string>"
        );
        let arguments: Vec<&str> = text
            .split("<array>")
            .nth(1)
            .and_then(|rest| rest.split("</array>").next())
            .expect("arguments")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(
            arguments,
            [
                "<string>/Users/someone/Library/Application Support/CodeFusion Runner/bin/codefusion-runner</string>",
                "<string>daemon</string>",
                "<string>--config</string>",
                "<string>/Users/someone/.config/codefusion-runner/config.json</string>",
            ]
        );
        // Nothing that lowers the priority of the Runner or of the vm-hosts it starts.
        for key in ["Nice", "LowPriorityIO", "LowPriorityBackgroundIO", "Background"] {
            assert!(!text.contains(key), "{key}");
        }
    }

    #[test]
    fn a_path_with_markup_in_it_stays_one_string() {
        let text = plist(Path::new("/a&b/<c>"), Path::new("/config"), Path::new("/state"));
        assert!(text.contains("<string>/a&amp;b/&lt;c&gt;</string>"), "{text}");
    }

    /// A launchd that has the agent or not, and writes down what it was asked.
    struct Fake {
        loaded: RefCell<bool>,
        calls: RefCell<Vec<String>>,
        /// How many `bootstrap` calls fail first.
        busy: RefCell<u32>,
        /// How many `print` calls still find the agent after a `bootout`.
        lingers: RefCell<u32>,
    }

    impl Fake {
        fn new(loaded: bool) -> Fake {
            Fake {
                loaded: RefCell::new(loaded),
                calls: RefCell::default(),
                busy: RefCell::new(0),
                lingers: RefCell::new(0),
            }
        }

        fn run(&self, args: &[&str]) -> Result<String, String> {
            self.calls.borrow_mut().push(args.join(" "));
            match args[0] {
                "print" => {
                    if *self.loaded.borrow() {
                        return Ok("state = running".into());
                    }
                    let mut lingers = self.lingers.borrow_mut();
                    if *lingers > 0 {
                        *lingers -= 1;
                        return Ok("state = exiting".into());
                    }
                    Err("Could not find service".into())
                }
                "bootout" => {
                    *self.loaded.borrow_mut() = false;
                    Ok(String::new())
                }
                "bootstrap" => {
                    let mut busy = self.busy.borrow_mut();
                    if *busy > 0 {
                        *busy -= 1;
                        return Err("Bootstrap failed: 5: Input/output error".into());
                    }
                    *self.loaded.borrow_mut() = true;
                    Ok(String::new())
                }
                other => Err(format!("unexpected {other}")),
            }
        }
    }

    fn agent<'a>(home: &'a Path, run: Launchctl<'a>) -> Agent<'a> {
        Agent {
            home,
            uid: 501,
            run,
            pause: Duration::ZERO,
        }
    }

    #[test]
    fn a_first_start_writes_the_list_and_loads_it_into_the_persons_session() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new(false);
        let run = |args: &[&str]| fake.run(args);
        let agent = agent(home.path(), &run);
        agent.start("<plist/>").expect("started");
        let path = home
            .path()
            .join("Library/LaunchAgents/cc.codefusion.vm-runner.plist");
        assert_eq!(std::fs::read_to_string(&path).expect("list"), "<plist/>");
        assert_eq!(
            *fake.calls.borrow(),
            [
                "print gui/501/cc.codefusion.vm-runner".to_string(),
                format!("bootstrap gui/501 {}", path.display()),
            ]
        );
    }

    #[test]
    fn a_start_over_a_running_agent_ends_it_first_and_waits_for_launchd() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new(true);
        *fake.lingers.borrow_mut() = 2;
        *fake.busy.borrow_mut() = 3;
        let run = |args: &[&str]| fake.run(args);
        agent(home.path(), &run).start("<plist/>").expect("started");
        let calls = fake.calls.borrow();
        assert_eq!(calls[0], "print gui/501/cc.codefusion.vm-runner");
        assert_eq!(calls[1], "bootout gui/501/cc.codefusion.vm-runner");
        assert_eq!(calls.iter().filter(|call| call.starts_with("print")).count(), 4);
        assert_eq!(
            calls.iter().filter(|call| call.starts_with("bootstrap")).count(),
            4
        );
        assert!(*fake.loaded.borrow());
    }

    #[test]
    fn a_launchd_that_never_takes_the_agent_is_an_error_with_its_words() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new(false);
        *fake.busy.borrow_mut() = 100;
        let run = |args: &[&str]| fake.run(args);
        let error = agent(home.path(), &run).start("<plist/>").expect_err("refused");
        assert_eq!(error, "Bootstrap failed: 5: Input/output error");
        assert_eq!(
            fake.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("bootstrap"))
                .count(),
            10
        );
    }

    #[test]
    fn removing_ends_the_agent_and_its_list_and_nothing_there_is_no_error() {
        let home = tempfile::tempdir().expect("home");
        let fake = Fake::new(false);
        let run = |args: &[&str]| fake.run(args);
        let agent = agent(home.path(), &run);
        agent.remove().expect("nothing to remove");
        assert_eq!(*fake.calls.borrow(), ["print gui/501/cc.codefusion.vm-runner"]);
        agent.start("<plist/>").expect("started");
        agent.remove().expect("removed");
        assert!(!agent.path().exists());
        assert!(!*fake.loaded.borrow());
        // Only this agent is ever named: the cf-runners manager's is never touched.
        assert!(
            fake.calls
                .borrow()
                .iter()
                .all(|call| !call.contains("cc.codefusion.runners"))
        );
    }
}
