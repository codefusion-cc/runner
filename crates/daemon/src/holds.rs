//! Holds set on the machine itself: `codefusion-runner pause --hold <name>` and `resume --hold <name>`, for work
//! that needs the host to itself for a while (a bot run, a stress run, a measurement, a deploy of what else it
//! hosts). The host takes no new job while any hold is set; running jobs finish. The console's pause is another
//! flag: the host takes jobs only while neither holds it, and the console's resume lifts no hold.
//!
//! Each hold is one file, `<state>/holds/<name>.json`, written by the Runner's own user in a folder only it can
//! open, so no other user can set or lift one. A file is put in place whole (a temporary name, then a rename), so a
//! crash leaves a hold set or not, never half of one. A file that cannot be read still holds: a hold is never lost
//! to a bad write. Two holders of different names never touch each other's file; two of one name share the hold.
//!
//! The daemon reads the folder at its start, before any listener opens, and on every tick, and writes what it sees
//! with its machines to `<state>/status.json` ([`LocalStatus`]), which `pause --wait` and `status --json` read.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The folder of the holds in a state folder.
pub fn holds_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("holds")
}

/// The status the daemon writes in a state folder.
pub fn status_file(state_dir: &Path) -> PathBuf {
    state_dir.join("status.json")
}

/// The lock a running Runner holds in a state folder.
pub fn lock_file(state_dir: &Path) -> PathBuf {
    state_dir.join("runner.lock")
}

/// How often at most the status goes unwritten while nothing in it changes, so a reader can tell a Runner that
/// writes it from one that stopped.
pub const STATUS_EVERY: Duration = Duration::from_secs(10);

/// The longest name of a hold.
pub const NAME_MAX: usize = 32;

/// The longest reason a hold keeps, in characters; a longer one is cut.
pub const REASON_MAX: usize = 200;

/// Whether `name` may name a hold: 1 to [`NAME_MAX`] of `a`-`z`, `0`-`9` and `-`. Nothing else, so a name is a
/// file name and never a path.
pub fn is_name(name: &str) -> bool {
    (1..=NAME_MAX).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// [`is_name`], or what a name must be: an error a command shows as it is.
pub fn check_name(name: &str) -> Result<(), String> {
    if is_name(name) {
        Ok(())
    } else {
        Err(format!(
            "{name:?} names no hold: 1 to {NAME_MAX} of a-z, 0-9 and -"
        ))
    }
}

/// One hold, as its file keeps it and the status and the console's report show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    /// Its name ([`is_name`]).
    pub name: String,
    /// Why it was set, as its holder said; none when it said nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When it was first set, RFC 3339 to the millisecond. Setting it again keeps this.
    pub since: String,
}

/// What the daemon last saw, for the commands on the machine: the holds, the console's pause, and the machines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalStatus {
    /// When it was written, RFC 3339 to the millisecond.
    pub at: String,
    /// The daemon's process.
    pub pid: u32,
    /// The Runner's version.
    pub version: String,
    /// Whether the host takes new jobs, and why not.
    pub open: bool,
    /// Why it does not, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Whether the console paused the host.
    pub console_paused: bool,
    /// The holds the daemon saw, by name: the host took no new job after it saw them.
    pub holds: Vec<Hold>,
    /// The machines that stand, each with its job.
    pub machines: Vec<StatusMachine>,
    /// Jobs that may still get a machine although no new slot is granted: slots promised to GitHub, or granted,
    /// with no machine yet.
    pub coming: usize,
    /// How many more machines of each size fit now; none for a size the host never runs.
    pub sizes: Vec<StatusSize>,
}

impl LocalStatus {
    /// Whether nothing runs and nothing more may start: no machine stands and no slot is promised.
    pub fn idle(&self) -> bool {
        self.machines.is_empty() && self.coming == 0
    }
}

/// One machine in [`LocalStatus`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMachine {
    /// Its Actions runner.
    pub runner: String,
    /// Its size's label, when it has one the Runner knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// The lifecycle's phase, in a word.
    pub state: String,
    /// When it was asked for, RFC 3339.
    pub started: String,
}

/// One size in [`LocalStatus`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSize {
    /// Its label.
    pub size: String,
    /// How many more fit now; none for a size the host never runs.
    pub free: Option<u32>,
}

/// A moment as the holds and the status write it: RFC 3339, UTC, to the millisecond.
pub fn moment(at: SystemTime) -> String {
    let at = OffsetDateTime::from(at);
    let at = at
        .replace_nanosecond(at.millisecond() as u32 * 1_000_000)
        .unwrap_or(at);
    at.format(&Rfc3339).unwrap_or_default()
}

/// Sets the hold `name`, or sets it again with this reason, keeping when it was first set. Errors are the file
/// system's, with the path: the hold is then not set, or set as before.
pub fn set(dir: &Path, name: &str, reason: Option<&str>, now: SystemTime) -> Result<Hold, String> {
    check_name(name)?;
    let said = |error: std::io::Error| format!("{}: {error}", dir.display());
    cf_secret::private_dir(dir).map_err(said)?;
    let path = dir.join(format!("{name}.json"));
    let since = match read_one(&path) {
        Some(Ok(hold)) => hold.since,
        _ => moment(now),
    };
    let hold = Hold {
        name: name.to_string(),
        reason: reason.and_then(clean),
        since,
    };
    let mut bytes = serde_json::to_vec(&hold).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    cf_secret::replace_private(&path, &bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(hold)
}

/// Lifts the hold `name`. One that is not set is no error: a holder's cleanup may run twice. Says whether it was
/// set.
pub fn lift(dir: &Path, name: &str) -> Result<bool, String> {
    check_name(name)?;
    let path = dir.join(format!("{name}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// The holds set, by name. A folder that is not there holds nothing. A hold's file that cannot be read or makes no
/// sense still holds, with a reason that says so; one that cannot even be listed is a hold of its own, `unreadable`:
/// a host that cannot tell whether it is held takes no job.
pub fn read(dir: &Path) -> Vec<Hold> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![unreadable("unreadable", &format!("{}: {error}", dir.display()))],
    };
    let mut holds = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                holds.push(unreadable("unreadable", &format!("{}: {error}", dir.display())));
                continue;
            }
        };
        let file = entry.file_name();
        let Some(name) = file.to_str().and_then(|file| file.strip_suffix(".json")) else {
            continue;
        };
        // What is not a hold's name is no hold: a temporary file (they start with a dot), or something else.
        if !is_name(name) {
            continue;
        }
        holds.push(match read_one(&entry.path()) {
            // A reason as `set` keeps it, whoever wrote the file: the console refuses a report with a longer one.
            Some(Ok(hold)) if hold.name == name => Hold {
                reason: hold.reason.as_deref().and_then(clean),
                ..hold
            },
            Some(Ok(_)) => unreadable(name, "its file names another hold"),
            Some(Err(why)) => unreadable(name, &why),
            // Lifted between the listing and the read.
            None => continue,
        });
    }
    holds.sort_by(|a, b| a.name.cmp(&b.name));
    holds.dedup_by(|a, b| a.name == b.name);
    holds
}

/// The holds' names, `bots, stress`, for a log or a line on the terminal.
pub fn names(holds: &[Hold]) -> String {
    holds
        .iter()
        .map(|hold| hold.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a status written at `at` ([`moment`]) is fresh at `now`: written no more than three times
/// [`STATUS_EVERY`] before, and not after it by more than a little clock drift. A moment that cannot be read is not.
pub fn fresh(at: &str, now: SystemTime) -> bool {
    let Ok(at) = OffsetDateTime::parse(at, &Rfc3339) else {
        return false;
    };
    let age = OffsetDateTime::from(now) - at;
    age >= -time::Duration::seconds(5) && age <= time::Duration::seconds(3 * STATUS_EVERY.as_secs() as i64)
}

/// The status the daemon wrote; none before its first, or one that cannot be read.
pub fn read_status(path: &Path) -> Option<LocalStatus> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes the status for the commands on the machine.
pub fn write_status(path: &Path, status: &LocalStatus) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(status).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    cf_secret::replace_private(path, &bytes)
}

/// A hold's file, read: none when it is not there.
fn read_one(path: &Path) -> Option<Result<Hold, String>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            Some(serde_json::from_slice(&bytes).map_err(|error| format!("its file makes no sense: {error}")))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(Err(format!("its file cannot be read: {error}"))),
    }
}

fn unreadable(name: &str, why: &str) -> Hold {
    Hold {
        name: name.to_string(),
        reason: Some(cf_guest_proto::clip(why, REASON_MAX)),
        since: String::new(),
    }
}

/// A reason as a hold keeps it: at most [`REASON_MAX`] characters, none that could steer how the console or a
/// terminal shows it, none at all when nothing but blanks is left.
fn clean(reason: &str) -> Option<String> {
    let reason = cf_guest_proto::clip(reason.trim(), REASON_MAX);
    let reason = reason.trim();
    (!reason.is_empty()).then(|| reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn a_status_is_fresh_for_three_of_its_intervals_and_a_little_drift() {
        let now = at(1_800_000_000);
        let ago = |seconds: u64| moment(now - Duration::from_secs(seconds));
        assert!(fresh(&ago(0), now));
        assert!(fresh(&ago(30), now));
        assert!(!fresh(&ago(31), now));
        assert!(fresh(&moment(now + Duration::from_secs(5)), now));
        assert!(!fresh(&moment(now + Duration::from_secs(6)), now));
        assert!(!fresh("yesterday", now));
        assert!(!fresh("", now));
    }

    #[test]
    fn names_are_short_words_of_lowercase_letters_digits_and_dashes() {
        for name in [
            "bots",
            "stress",
            "measure",
            "deploy",
            "a",
            "0",
            "-",
            &"a".repeat(32),
        ] {
            assert!(is_name(name), "{name}");
        }
        for name in [
            "",
            "Bots",
            "bots.json",
            "../x",
            "a/b",
            "a b",
            "ü",
            &"a".repeat(33),
            ".x",
        ] {
            assert!(!is_name(name), "{name}");
        }
    }

    #[test]
    fn a_hold_set_is_read_until_it_is_lifted_and_lifting_one_not_set_is_fine() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        assert_eq!(read(&dir), vec![], "no folder holds nothing");
        let hold = set(&dir, "bots", Some(" bot run 42 "), at(1_800_000_000)).expect("set");
        assert_eq!(hold.reason.as_deref(), Some("bot run 42"));
        assert_eq!(hold.since, "2027-01-15T08:00:00Z");
        assert_eq!(read(&dir), vec![hold]);
        assert_eq!(lift(&dir, "bots"), Ok(true));
        assert_eq!(read(&dir), vec![]);
        assert_eq!(lift(&dir, "bots"), Ok(false));
        assert_eq!(lift(&dir, "never"), Ok(false));
    }

    #[test]
    fn setting_a_hold_again_keeps_when_it_began_and_takes_the_new_reason() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        set(&dir, "stress", Some("first"), at(100)).expect("set");
        let again = set(&dir, "stress", None, at(200)).expect("set again");
        assert_eq!(again.since, moment(at(100)));
        assert_eq!(again.reason, None);
        assert_eq!(read(&dir), vec![again]);
    }

    #[test]
    fn two_holds_hold_until_both_are_lifted() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        set(&dir, "stress", None, at(1)).expect("set");
        set(&dir, "bots", None, at(2)).expect("set");
        let names = |dir: &Path| read(dir).into_iter().map(|hold| hold.name).collect::<Vec<_>>();
        assert_eq!(names(&dir), ["bots", "stress"]);
        lift(&dir, "bots").expect("lifted");
        assert_eq!(names(&dir), ["stress"]);
        lift(&dir, "stress").expect("lifted");
        assert!(names(&dir).is_empty());
    }

    #[test]
    fn bad_names_are_refused_and_touch_nothing() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        for name in ["../pause", "", "Bots", "a/b"] {
            assert!(set(&dir, name, None, at(1)).is_err(), "{name}");
            assert!(lift(&dir, name).is_err(), "{name}");
        }
        assert!(!dir.exists() || read(&dir).is_empty());
        assert!(!folder.path().join("pause").exists());
    }

    #[test]
    fn a_hold_whose_file_is_damaged_still_holds_and_says_why() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        set(&dir, "bots", None, at(1)).expect("set");
        std::fs::write(dir.join("bots.json"), "{\"name\": \"bo").expect("half a file");
        std::fs::write(dir.join("measure.json"), r#"{"name":"other","since":""}"#).expect("a wrong name");
        let holds = read(&dir);
        assert_eq!(holds.len(), 2, "{holds:?}");
        assert_eq!(holds[0].name, "bots");
        assert!(
            holds[0].reason.as_deref().unwrap_or("").contains("no sense"),
            "{holds:?}"
        );
        assert_eq!(holds[1].name, "measure");
        assert!(
            holds[1].reason.as_deref().unwrap_or("").contains("another hold"),
            "{holds:?}"
        );
        // Lifting it lifts it all the same.
        assert_eq!(lift(&dir, "bots"), Ok(true));
    }

    #[test]
    fn what_a_crash_leaves_beside_the_holds_is_no_hold() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        set(&dir, "deploy", None, at(1)).expect("set");
        std::fs::write(dir.join(".deploy.json.123.4.partial"), "{").expect("a temporary file");
        std::fs::write(dir.join("notes.txt"), "x").expect("something else");
        std::fs::write(dir.join("Upper.json"), "{}").expect("no name of a hold");
        let names: Vec<String> = read(&dir).into_iter().map(|hold| hold.name).collect();
        assert_eq!(names, ["deploy"]);
    }

    #[test]
    fn the_folder_and_each_hold_are_the_runner_users_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        set(&dir, "bots", None, at(1)).expect("set");
        let mode = |path: &Path| std::fs::metadata(path).expect("there").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("bots.json")), 0o600);
    }

    #[test]
    fn a_reason_is_cut_and_its_control_characters_are_blanked() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        let long = format!("a\u{1b}[31m{}", "x".repeat(500));
        let hold = set(&dir, "bots", Some(&long), at(1)).expect("set");
        let reason = hold.reason.expect("a reason");
        assert_eq!(reason.chars().count(), REASON_MAX);
        assert!(!reason.contains('\u{1b}'));
    }

    #[test]
    fn a_reason_written_by_hand_is_read_back_cut() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        set(&dir, "bots", None, at(1)).expect("set");
        let long = format!(
            "{{\"name\":\"bots\",\"reason\":\"a\\u001b{}\",\"since\":\"x\"}}",
            "x".repeat(500)
        );
        std::fs::write(dir.join("bots.json"), long).expect("written by hand");
        let reason = read(&dir)[0].reason.clone().expect("a reason");
        assert_eq!(reason.chars().count(), REASON_MAX);
        assert!(!reason.contains('\u{1b}'));
    }

    #[test]
    fn many_holders_of_one_name_at_once_leave_one_whole_hold() {
        let folder = tempfile::tempdir().expect("a folder");
        let dir = holds_dir(folder.path());
        std::thread::scope(|scope| {
            for i in 0..16 {
                let dir = &dir;
                scope.spawn(move || {
                    for _ in 0..20 {
                        set(dir, "bots", Some(&format!("holder {i}")), at(1)).expect("set");
                        let holds = read(dir);
                        assert_eq!(holds.len(), 1, "{holds:?}");
                        assert!(
                            holds[0]
                                .reason
                                .as_deref()
                                .is_some_and(|r| r.starts_with("holder")),
                            "{holds:?}"
                        );
                    }
                });
            }
        });
        let left: Vec<_> = std::fs::read_dir(&dir).expect("listed").collect();
        assert_eq!(left.len(), 1, "no temporary file stays behind");
    }
}
