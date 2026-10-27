//! The Runner's configuration file: where its state is, which GitHub App and console it talks to, what the host
//! is for. One JSON file that the service manager's entry names; nothing in it is a secret.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cf_budget::{Limits, Millis, Mode, Settings};
use cf_console_client::Os;
use cf_daemon::names::slug;
use serde::Deserialize;

/// The label of the cf-runners manager's launchd agent, which the Runner may run beside and never touches.
#[cfg(target_os = "macos")]
pub const OLD_MANAGER_LABEL: &str = "cc.codefusion.runners";

/// What a person works on the Mac means for jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Use {
    /// Nobody works here: jobs run at any time.
    Dedicated,
    /// Someone's own Mac: jobs start only while the person is away.
    Workstation,
}

/// The configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// What this Mac is called at GitHub and at the console: lowercase letters, digits and single dashes, at most
    /// 24. Its scale sets and Actions runners carry the name, so it must be no other Runner's or manager's.
    pub machine: String,
    /// The folder with everything the Runner keeps: images, job disks, records, the outbox, logs. On the
    /// internal disk: a launchd agent may not open an external volume.
    pub state_dir: PathBuf,
    /// The folder with the GitHub App's `app.json` and `app.pem`.
    pub app_dir: PathBuf,
    /// GitHub's API.
    #[serde(default = "github_api")]
    pub github_api: String,
    /// The version of the installed image jobs run in.
    pub image: String,
    /// Serve only these `owner/name` repositories; empty serves every private repository the App can see.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Public repositories to serve on purpose.
    #[serde(default)]
    pub public_repos: Vec<String>,
    /// The repositories whose `cf-docker` jobs get a Docker daemon of their own. Private ones only: a public one
    /// listed here is not given Docker.
    #[serde(default)]
    pub docker_repos: Vec<String>,
    /// The most Docker jobs at once: a daemon and its builds take more of the Mac's processors than other jobs.
    #[serde(default = "docker_max_jobs")]
    pub docker_max_jobs: usize,
    /// How many GB jobs' caches may hold together, one per repository; 0 keeps none.
    #[serde(default = "cache_gb")]
    pub cache_gb: u64,
    /// The console's address, when the Mac reports to one.
    #[serde(default)]
    pub console: Option<String>,
    /// Whether a person works on the Mac. Required on a Mac; on Linux, where nobody sits at the host, it is
    /// `dedicated` or left out.
    #[serde(rename = "use")]
    #[cfg_attr(not(target_os = "macos"), serde(default = "dedicated"))]
    pub mac_use: Use,
    /// The longest a job may run, in minutes.
    #[serde(default = "deadline_minutes")]
    pub deadline_minutes: u64,
    /// The most jobs at once, 1 to 32; left out for as many as memory and CPUs let run.
    #[serde(default)]
    pub max_jobs: Option<u32>,
    /// Minutes without a key or the mouse before a workstation takes jobs, 1 to 1440; left out for 15.
    #[serde(default)]
    pub idle_minutes: Option<u32>,
    /// The memory the host keeps for itself, in percent of the installed memory: 1 to 90, and at least 1 GB. Left out
    /// for the default, 2 GB on a 16 GB Mac nobody works at.
    #[serde(default)]
    pub min_free_memory_percent: Option<u32>,
    /// The git checkout that belongs to the Runner alone, which the console's `update` fetches, checks out at a
    /// commit and builds in. Without it the Runner is updated by hand: a person's checkout is never touched.
    #[serde(default)]
    pub build_folder: Option<PathBuf>,
    /// The host labels this host has, 8 at most: a job that asks for host labels (`runs-on: [cf-8g, cf-macos]`)
    /// runs only on a host that has every one of them. Left out for its system's: `cf-macos` on a Mac, `cf-linux`
    /// on Linux.
    #[serde(default = "labels")]
    pub labels: Vec<String>,
}

fn github_api() -> String {
    "https://api.github.com".into()
}

const fn deadline_minutes() -> u64 {
    360
}

const fn docker_max_jobs() -> usize {
    1
}

const fn cache_gb() -> u64 {
    30
}

fn labels() -> Vec<String> {
    match Os::this() {
        Some(Os::Macos) => vec!["cf-macos".into()],
        Some(Os::Linux) => vec!["cf-linux".into()],
        None => Vec::new(),
    }
}

#[cfg(not(target_os = "macos"))]
const fn dedicated() -> Use {
    Use::Dedicated
}

impl Config {
    /// Reads and checks the file. One that others can write is refused: it says where the console's token and the
    /// App's key go.
    pub fn load(path: &Path) -> Result<Config, String> {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o022 != 0 {
            return Err(format!(
                "{}: others can write it (mode {:o}); `chmod go-w` it",
                path.display(),
                mode & 0o777
            ));
        }
        let text = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        Config::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// Reads and checks the file's text.
    pub fn parse(text: &str) -> Result<Config, String> {
        let config: Config = serde_json::from_str(text).map_err(|error| error.to_string())?;
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<(), String> {
        if self.machine.is_empty() || slug(&self.machine, 24) != self.machine || self.machine.contains("--") {
            return Err(format!(
                "machine: `{}` is no name of lowercase letters, digits and single dashes, 24 at most",
                self.machine
            ));
        }
        let build_folder = self.build_folder.iter().map(|folder| ("build_folder", folder));
        let folders = [("state_dir", &self.state_dir), ("app_dir", &self.app_dir)];
        for (what, folder) in folders.into_iter().chain(build_folder.clone()) {
            if !folder.is_absolute() {
                return Err(format!("{what}: `{}` is not an absolute path", folder.display()));
            }
        }
        // The service reads and writes the state folder, and `update` builds in the build folder.
        #[cfg(target_os = "macos")]
        for (what, folder) in std::iter::once(("state_dir", &self.state_dir)).chain(build_folder) {
            if folder.starts_with("/Volumes") {
                return Err(format!(
                    "{what}: `{}` is on an external volume, which a launchd agent may not open",
                    folder.display()
                ));
            }
        }
        #[cfg(not(target_os = "macos"))]
        if self.mac_use == Use::Workstation {
            return Err(
                "use: `workstation` means something on macOS only, where a person works at the Mac; say \
                 `dedicated` or leave `use` out"
                    .into(),
            );
        }
        if self.image.is_empty() {
            return Err("image: no version".into());
        }
        if !(1..=24 * 60).contains(&self.deadline_minutes) {
            return Err("deadline_minutes: between 1 and 1440".into());
        }
        for url in std::iter::once(&self.github_api).chain(&self.console) {
            // The App client's own rule for where a token may go: HTTPS, or this Mac itself.
            if !cf_ghapp::is_safe_address_text(url) {
                return Err(format!("`{url}` is no https address"));
            }
        }
        if self.max_jobs.is_some_and(|jobs| !(1..=32).contains(&jobs)) {
            return Err("max_jobs: between 1 and 32".into());
        }
        if self
            .idle_minutes
            .is_some_and(|minutes| !(1..=24 * 60).contains(&minutes))
        {
            return Err("idle_minutes: between 1 and 1440".into());
        }
        if self.idle_minutes.is_some() && self.mac_use != Use::Workstation {
            return Err(
                "idle_minutes: only a workstation waits for its person; a dedicated host takes jobs at any time"
                    .into(),
            );
        }
        if self
            .min_free_memory_percent
            .is_some_and(|percent| !Settings::FLOOR_PERCENT.contains(&percent))
        {
            return Err(format!(
                "min_free_memory_percent: between {} and {}",
                Settings::FLOOR_PERCENT.start(),
                Settings::FLOOR_PERCENT.end()
            ));
        }
        if self.labels.len() > 8 {
            return Err("labels: 8 at most".into());
        }
        for (i, label) in self.labels.iter().enumerate() {
            if !cf_workflows::is_host_label(label) {
                return Err(format!(
                    "labels: `{label}` is no host label: {}",
                    cf_workflows::HOST_LABEL_RULE
                ));
            }
            if self.labels[..i].contains(label) {
                return Err(format!("labels: `{label}` is listed twice"));
            }
        }
        if !(1..=8).contains(&self.docker_max_jobs) {
            return Err("docker_max_jobs: between 1 and 8".into());
        }
        if self.cache_gb > 1000 {
            return Err("cache_gb: 0 (no cache) to 1000".into());
        }
        for repo in self
            .repos
            .iter()
            .chain(&self.public_repos)
            .chain(&self.docker_repos)
        {
            let named = repo
                .split_once('/')
                .is_some_and(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'));
            if !named {
                return Err(format!("`{repo}` is no `owner/name` repository"));
            }
        }
        Ok(())
    }

    /// The budget's mode.
    pub const fn mode(&self) -> Mode {
        match self.mac_use {
            Use::Dedicated => Mode::Dedicated,
            Use::Workstation => Mode::Workstation,
        }
    }

    /// What the file sets of the budget on top of its defaults.
    pub fn limits(&self) -> Limits {
        Limits {
            max_machines: self.max_jobs,
            idle_after: self.idle_minutes.map(|minutes| Millis::mins(minutes.into())),
            floor_percent: self.min_free_memory_percent,
        }
    }

    /// The longest a job may run.
    pub const fn deadline(&self) -> Duration {
        Duration::from_secs(self.deadline_minutes * 60)
    }

    /// Where the records of machines are kept.
    pub fn records(&self) -> PathBuf {
        cf_daemon::records_dir(&self.state_dir)
    }

    /// Where this Mac's token for the console is kept, readable by its owner alone.
    pub fn console_token(&self) -> PathBuf {
        self.state_dir.join("console-token")
    }

    /// Where the service writes its log.
    pub fn log(&self) -> PathBuf {
        self.state_dir.join("logs").join("runner.log")
    }

    /// The installed program: a copy of its own, so that a build folder can go away.
    pub fn program(&self) -> PathBuf {
        self.state_dir.join("bin").join("codefusion-runner")
    }
}

/// The name the cf-runners manager has on this Mac, when it is installed for this user: the `machine` of its
/// configuration, or the name it makes of the host name. The Runner must not carry it: both name their scale sets
/// `<machine>--…`, each deletes those of its name it does not want, and their sessions would shut each other out.
#[cfg(target_os = "macos")]
pub fn old_manager_name(home: &Path, hostname: &str) -> Option<String> {
    let agent = home
        .join("Library/LaunchAgents")
        .join(format!("{OLD_MANAGER_LABEL}.plist"));
    if !agent.exists() {
        return None;
    }
    #[derive(Deserialize)]
    struct Old {
        #[serde(default)]
        machine: String,
    }
    let configured = std::fs::read_to_string(home.join(".config/codefusion-runners/config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Old>(&text).ok())
        .map(|old| old.machine)
        .filter(|machine| !machine.is_empty());
    Some(configured.unwrap_or_else(|| cf_daemon::names::machine_name(hostname)))
}

/// Refuses a machine name the cf-runners manager on this Mac has.
#[cfg(target_os = "macos")]
pub fn check_beside_old_manager(config: &Config, home: &Path, hostname: &str) -> Result<(), String> {
    match old_manager_name(home, hostname) {
        Some(old) if old == config.machine => Err(format!(
            "machine: the cf-runners manager on this Mac is called `{old}` too; give the Runner another name \
             (`{old}-vm`), or the two delete each other's scale sets"
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHOLE: &str = r#"{
        "machine": "mac-a-vm",
        "state_dir": "/Users/someone/Library/Application Support/CodeFusion Runner",
        "app_dir": "/Users/someone/.config/codefusion-runners",
        "image": "2026.10.1",
        "use": "dedicated"
    }"#;

    fn with(change: impl FnOnce(&mut serde_json::Value)) -> Result<Config, String> {
        let mut value: serde_json::Value = serde_json::from_str(WHOLE).expect("json");
        change(&mut value);
        Config::parse(&value.to_string())
    }

    #[test]
    fn a_file_others_can_write_is_not_read() {
        use std::os::unix::fs::PermissionsExt as _;
        let folder = tempfile::tempdir().expect("folder");
        let path = folder.path().join("config.json");
        std::fs::write(&path, WHOLE).expect("a file");
        let set = |mode: u32| {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("a mode");
        };
        // It says where the console's token and the App's key go: whoever writes it has them.
        for mode in [0o666, 0o664, 0o646] {
            set(mode);
            let error = Config::load(&path).expect_err("refused");
            assert!(
                error.contains("others can write it") && error.contains(&path.display().to_string()),
                "{mode:o}: {error}"
            );
        }
        for mode in [0o644, 0o600, 0o444] {
            set(mode);
            Config::load(&path).unwrap_or_else(|error| panic!("{mode:o}: {error}"));
        }
    }

    #[test]
    fn the_smallest_file_has_the_defaults() {
        let config = Config::parse(WHOLE).expect("config");
        assert_eq!(config.github_api, "https://api.github.com");
        assert_eq!(config.deadline(), Duration::from_secs(6 * 3600));
        assert_eq!(config.mode(), Mode::Dedicated);
        assert_eq!(config.console, None);
        assert_eq!(config.repos, Vec::<String>::new());
        assert_eq!(
            config.program(),
            Path::new("/Users/someone/Library/Application Support/CodeFusion Runner/bin/codefusion-runner")
        );
    }

    #[test]
    fn a_field_nobody_knows_is_a_mistake_and_not_ignored() {
        let error = with(|v| v["mashine"] = "mac-b".into()).expect_err("refused");
        assert!(error.contains("mashine"), "{error}");
    }

    #[test]
    fn a_machine_name_is_a_slug_of_24_at_most() {
        for name in [
            "",
            "Mac-A",
            "mac a",
            "mac--a",
            "-mac",
            "mac-",
            "a-name-that-is-longer-than-24",
            "mäc",
        ] {
            let error = with(|v| v["machine"] = name.into()).expect_err(name);
            assert!(error.starts_with("machine:"), "{name}: {error}");
        }
        for name in ["m", "mac-a-vm", "a-name-of-twenty-four-ch"] {
            assert_eq!(
                with(|v| v["machine"] = name.into()).map(|c| c.machine),
                Ok(name.to_string())
            );
        }
    }

    #[test]
    fn the_state_folder_is_absolute_and_on_a_mac_on_no_external_volume() {
        assert!(with(|v| v["state_dir"] = "state".into()).is_err());
        assert!(with(|v| v["app_dir"] = "~/.config/x".into()).is_err());
        let external = with(|v| v["state_dir"] = "/Volumes/External/runner".into());
        if cfg!(target_os = "macos") {
            let error = external.expect_err("refused");
            assert!(error.contains("external volume"), "{error}");
        } else {
            // A folder of that name is nothing special on Linux.
            assert!(external.is_ok(), "{external:?}");
        }
    }

    #[test]
    fn what_the_host_is_for_is_said_on_a_mac_and_may_be_left_out_on_linux_where_nobody_works_at_it() {
        let without = with(|v| {
            v.as_object_mut().expect("an object").remove("use");
        });
        let workstation = with(|v| v["use"] = "workstation".into());
        if cfg!(target_os = "macos") {
            let error = without.expect_err("required");
            assert!(error.contains("use"), "{error}");
            assert_eq!(workstation.map(|c| c.mode()), Ok(Mode::Workstation));
        } else {
            assert_eq!(without.map(|c| c.mode()), Ok(Mode::Dedicated));
            let error = workstation.expect_err("no person works at a Linux host");
            assert!(
                error.starts_with("use: `workstation` means something on macOS only"),
                "{error}"
            );
        }
        // `dedicated` is read on both, and a word it does not know is refused on both.
        assert_eq!(with(|_| ()).map(|c| c.mode()), Ok(Mode::Dedicated));
        assert!(with(|v| v["use"] = "sometimes".into()).is_err());
    }

    #[test]
    fn a_build_folder_is_optional_absolute_and_on_no_external_volume() {
        assert_eq!(with(|_| ()).expect("no build folder").build_folder, None);
        let config =
            with(|v| v["build_folder"] = "/Users/someone/runner-build".into()).expect("a build folder");
        assert_eq!(
            config.build_folder.as_deref(),
            Some(Path::new("/Users/someone/runner-build"))
        );
        let error = with(|v| v["build_folder"] = "runner-build".into()).expect_err("refused");
        assert!(
            error.starts_with("build_folder:") && error.contains("absolute"),
            "{error}"
        );
        if cfg!(target_os = "macos") {
            let error = with(|v| v["build_folder"] = "/Volumes/External/runner".into()).expect_err("refused");
            assert!(
                error.starts_with("build_folder:") && error.contains("external volume"),
                "{error}"
            );
        }
    }

    #[test]
    fn addresses_are_https_or_this_mac() {
        assert!(with(|v| v["console"] = "http://console.example".into()).is_err());
        assert!(with(|v| v["github_api"] = "ftp://api.github.com".into()).is_err());
        assert!(with(|v| v["console"] = "https://console.example".into()).is_ok());
        assert!(with(|v| v["console"] = "http://127.0.0.1:8787".into()).is_ok());
        for url in [
            "http://localhost.example",
            "http://127.0.0.1.example:8787",
            "http://localhost@example.com",
            "http://[::1]x",
            "http://localhost:80a",
            "http://192.168.1.10:8787",
            "https://",
        ] {
            assert!(with(|v| v["console"] = url.into()).is_err(), "{url}");
        }
        for url in [
            "http://localhost",
            "http://localhost:8787/x",
            "http://[::1]:8787",
            "http://[::1]/",
            "http://127.0.0.2:8787",
        ] {
            assert!(with(|v| v["console"] = url.into()).is_ok(), "{url}");
        }
    }

    #[test]
    fn a_deadline_is_between_a_minute_and_a_day_and_repositories_are_named_whole() {
        assert!(with(|v| v["deadline_minutes"] = 0.into()).is_err());
        assert!(with(|v| v["deadline_minutes"] = 1441.into()).is_err());
        assert_eq!(
            with(|v| v["deadline_minutes"] = 1440.into()).map(|c| c.deadline()),
            Ok(Duration::from_secs(86400))
        );
        for repo in ["app", "/app", "org/", "org/app/x"] {
            assert!(
                with(|v| v["repos"] = serde_json::json!([repo])).is_err(),
                "{repo}"
            );
        }
        assert!(with(|v| v["public_repos"] = serde_json::json!(["org/app"])).is_ok());
        assert!(with(|v| v["use"] = "sometimes".into()).is_err());
    }

    #[test]
    fn docker_is_for_no_repository_and_one_job_at_a_time_unless_set() {
        let config = Config::parse(WHOLE).expect("config");
        assert_eq!(config.docker_repos, Vec::<String>::new());
        assert_eq!(config.docker_max_jobs, 1);
        assert_eq!(config.cache_gb, 30);
        assert_eq!(with(|v| v["cache_gb"] = 0.into()).map(|c| c.cache_gb), Ok(0));
        assert!(with(|v| v["cache_gb"] = 1000.into()).is_ok());
        assert!(with(|v| v["cache_gb"] = 1001.into()).is_err());
        assert!(with(|v| v["cache_gb"] = (-1).into()).is_err());
        let set = with(|v| {
            v["docker_repos"] = serde_json::json!(["org/shop"]);
            v["docker_max_jobs"] = 2.into();
        })
        .expect("config");
        assert_eq!(set.docker_repos, ["org/shop"]);
        assert_eq!(set.docker_max_jobs, 2);
        for repo in ["shop", "org/", "/shop", "org/shop/x"] {
            assert!(
                with(|v| v["docker_repos"] = serde_json::json!([repo])).is_err(),
                "{repo}"
            );
        }
        for max in [0, 9] {
            assert!(with(|v| v["docker_max_jobs"] = max.into()).is_err(), "{max}");
        }
        assert!(with(|v| v["docker_max_jobs"] = 8.into()).is_ok());
    }

    #[test]
    fn a_hosts_labels_are_its_systems_unless_set_and_only_host_labels() {
        let system = if cfg!(target_os = "macos") {
            "cf-macos"
        } else {
            "cf-linux"
        };
        assert_eq!(with(|_| ()).expect("config").labels, [system]);
        let set = |labels: serde_json::Value| with(|v| v["labels"] = labels);
        assert_eq!(
            set(serde_json::json!(["cf-macos", "cf-studio"]))
                .expect("set")
                .labels,
            ["cf-macos", "cf-studio"]
        );
        assert_eq!(
            set(serde_json::json!([])).expect("none").labels,
            Vec::<String>::new(),
            "a host with none serves only what asks for none"
        );
        let eight: Vec<String> = (0..8).map(|i| format!("cf-host{i}")).collect();
        assert!(set(serde_json::json!(eight)).is_ok(), "eight");
        let nine: Vec<String> = (0..9).map(|i| format!("cf-host{i}")).collect();
        assert_eq!(
            set(serde_json::json!(nine)).err().as_deref(),
            Some("labels: 8 at most")
        );
        for label in [
            "cf-8g",
            "cf-docker",
            "macos",
            "CF-MACOS",
            "cf-",
            "self-hosted",
            "cf-mac os",
        ] {
            assert_eq!(
                set(serde_json::json!([label])).err(),
                Some(format!(
                    "labels: `{label}` is no host label: {}",
                    cf_workflows::HOST_LABEL_RULE
                )),
                "{label}"
            );
        }
        assert_eq!(
            set(serde_json::json!(["cf-macos", "cf-macos"])).err().as_deref(),
            Some("labels: `cf-macos` is listed twice")
        );
        assert!(
            set(serde_json::json!("cf-macos")).is_err(),
            "a list, not one label"
        );
    }

    #[test]
    fn the_limits_for_jobs_are_the_defaults_unless_set_and_set_only_within_reason() {
        assert_eq!(Config::parse(WHOLE).expect("config").limits(), Limits::default());
        assert_eq!(
            with(|v| {
                v["max_jobs"] = 2.into();
                v["min_free_memory_percent"] = 8.into();
            })
            .map(|c| c.limits()),
            Ok(Limits {
                max_machines: Some(2),
                idle_after: None,
                floor_percent: Some(8),
            })
        );
        // Idle minutes are a workstation's, and only a Mac is one.
        let mut bounds = vec![("max_jobs", 1, 32), ("min_free_memory_percent", 1, 90)];
        if cfg!(target_os = "macos") {
            assert_eq!(
                with(|v| {
                    v["use"] = "workstation".into();
                    v["idle_minutes"] = 20.into();
                })
                .map(|c| c.limits().idle_after),
                Ok(Some(Millis::mins(20)))
            );
            bounds.push(("idle_minutes", 1, 1440));
        }
        for (key, low, high) in bounds {
            let set = |value: i64| {
                with(|v| {
                    if key == "idle_minutes" {
                        v["use"] = "workstation".into();
                    }
                    v[key] = value.into();
                })
            };
            assert!(set(low).is_ok(), "{key} {low}");
            assert!(set(high).is_ok(), "{key} {high}");
            assert_eq!(
                set(low - 1).err(),
                Some(format!("{key}: between {low} and {high}"))
            );
            assert_eq!(
                set(high + 1).err(),
                Some(format!("{key}: between {low} and {high}"))
            );
            assert!(set(-1).is_err(), "{key} -1");
        }
        assert_eq!(
            with(|v| v["idle_minutes"] = 20.into()).err().as_deref(),
            Some(
                "idle_minutes: only a workstation waits for its person; a dedicated host takes jobs at any time"
            )
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_old_managers_name_is_refused_only_while_the_old_manager_is_installed() {
        let home = tempfile::tempdir().expect("home");
        let config = with(|v| v["machine"] = "build-mac-2".into()).expect("config");
        // No old manager: any name.
        assert_eq!(old_manager_name(home.path(), "Build-Mac-2.local"), None);
        assert_eq!(
            check_beside_old_manager(&config, home.path(), "Build-Mac-2.local"),
            Ok(())
        );
        // Installed, with the name it makes of the host name.
        let agents = home.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).expect("folder");
        std::fs::write(agents.join("cc.codefusion.runners.plist"), "").expect("plist");
        assert_eq!(
            old_manager_name(home.path(), "Build-Mac-2.local"),
            Some("build-mac-2".to_string())
        );
        let error = check_beside_old_manager(&config, home.path(), "Build-Mac-2.local").expect_err("refused");
        assert!(error.contains("`build-mac-2-vm`"), "{error}");
        let other = with(|v| v["machine"] = "build-mac-2-vm".into()).expect("config");
        assert_eq!(
            check_beside_old_manager(&other, home.path(), "Build-Mac-2.local"),
            Ok(())
        );
        // Installed, with a name of its own.
        let old = home.path().join(".config/codefusion-runners");
        std::fs::create_dir_all(&old).expect("folder");
        std::fs::write(
            old.join("config.json"),
            r#"{"machine": "build-mac-2-vm", "maxJobs": 2}"#,
        )
        .expect("config");
        assert!(check_beside_old_manager(&other, home.path(), "Build-Mac-2.local").is_err());
        assert_eq!(
            check_beside_old_manager(&config, home.path(), "Build-Mac-2.local"),
            Ok(())
        );
    }
}
