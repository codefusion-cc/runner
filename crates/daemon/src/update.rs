//! The Runner's updates of itself, which the console asks for with `update`.
//!
//! The service decides when ([`crate::Service`]): it builds while jobs run, holds new jobs until no machine
//! stands, then installs and ends, and the service manager starts the new program. How a build and an install go
//! on this host is a [`SelfUpdate`]'s. What is under way is written down ([`Pending`] in [`UPDATE_FILE`]) before the program is
//! replaced, so the program that starts next knows what it is: the new one counts its starts and goes back to the
//! one before when it does not stay up ([`at_start`]); the one before, started again, tells the console the update
//! failed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The file in the state folder with the update under way.
pub const UPDATE_FILE: &str = "update.json";

/// How often a new program may start without staying up before the one before is put back.
pub const MAX_STARTS: u32 = 3;

/// How long a new program must run before its update counts as done. A program that cannot start ends 30 s after it
/// began and is started again (launchd's `ThrottleInterval`, the Runner's own wait under systemd), so one that fails
/// at once uses its starts within two minutes.
pub const STAYS_UP: Duration = Duration::from_secs(60);

/// How the Runner updates itself on this host. Both calls take long and block: they run on a thread of their own.
pub trait SelfUpdate: Send + Sync {
    /// Where updates come from, for the console: the checkout and the branch.
    fn source(&self) -> String;

    /// Fetches the source and builds it. Answers the version of the program built and ready to install, or the
    /// running version when the source has nothing newer, in which case nothing was built.
    fn build(&self) -> Result<String, String>;

    /// Puts the program built last in place of the running one, keeping the running one to go back to, with
    /// `pending` written down first. Refused while a machine has a record.
    fn install(&self, pending: &Pending) -> Result<(), String>;
}

/// The Runner's settings file, which it reads when it starts: new settings take effect when it starts again.
pub trait SettingsFile: Send + Sync {
    /// What the file says now, against what the Runner started with. Reads the file; quick, but not async.
    fn look(&self) -> SettingsLook;
}

/// What a settings file says now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsLook {
    /// What the Runner started with.
    Same,
    /// Other settings, which a Runner could start on. `id` names this version of the file.
    Changed {
        /// This version of the file.
        id: String,
    },
    /// Other settings no Runner could start on, and why.
    Unusable {
        /// What a Runner starting on them would say.
        why: String,
    },
}

/// An update whose program was put in place and has not yet stayed up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The console's command, which the result names.
    pub id: String,
    /// The version that ran before.
    pub from: String,
    /// The version put in place.
    pub to: String,
    /// How often `to` started without staying up.
    #[serde(default)]
    pub starts: u32,
    /// Why the update ended without the program before back in place, for the Runner that starts next to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

/// A limit as a person says it: whole minutes, or seconds below one.
pub fn spoken(limit: Duration) -> String {
    match limit.as_secs() {
        1 => "1 second".into(),
        60..120 => "1 minute".into(),
        secs @ 120.. => format!("{} minutes", secs / 60),
        secs => format!("{secs} seconds"),
    }
}

/// What a program that starts does about an update under way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtStart {
    /// None is under way.
    Nothing,
    /// This is the new program: it runs with this written down first, and says the update is done once it stayed
    /// up for [`STAYS_UP`].
    Trying(Pending),
    /// This is the new program and it did not stay up [`MAX_STARTS`] times: the one before goes back in place and
    /// this one ends.
    GoBack(Pending),
    /// The update is over and did not take: what the console is told of its command.
    Failed {
        /// The command.
        id: String,
        /// What happened.
        message: String,
    },
}

/// What the program of version `running` does at its start, with `pending` as found.
pub fn at_start(pending: Option<Pending>, running: &str) -> AtStart {
    let Some(mut pending) = pending else {
        return AtStart::Nothing;
    };
    if let Some(message) = pending.failed {
        return AtStart::Failed {
            id: pending.id,
            message,
        };
    }
    if pending.to == running {
        pending.starts = pending.starts.saturating_add(1);
        return if pending.starts > MAX_STARTS {
            AtStart::GoBack(pending)
        } else {
            AtStart::Trying(pending)
        };
    }
    let message = if pending.from == running {
        format!(
            "{} did not stay up and was taken back: {running} runs again (logs/{} says why)",
            pending.to,
            <crate::This as crate::Platform>::SERVICE_LOG
        )
    } else {
        format!(
            "the update to {} was overtaken: {running} was installed meanwhile",
            pending.to
        )
    };
    AtStart::Failed {
        id: pending.id,
        message,
    }
}

/// [`UPDATE_FILE`] of a state folder, for this user alone.
#[derive(Clone, Debug)]
pub struct UpdateFile(PathBuf);

impl UpdateFile {
    /// The file in `state_dir`.
    pub fn in_state(state_dir: &Path) -> UpdateFile {
        UpdateFile(state_dir.join(UPDATE_FILE))
    }

    /// The update under way, if any. A file that cannot be read as one is an error.
    pub fn read(&self) -> Result<Option<Pending>, String> {
        match std::fs::read(&self.0) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| format!("{}: {error}", self.0.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", self.0.display())),
        }
    }

    /// Writes `pending` in one step.
    pub fn write(&self, pending: &Pending) -> Result<(), String> {
        let bytes = serde_json::to_vec(pending).map_err(|error| error.to_string())?;
        cf_secret::replace_private(&self.0, &bytes).map_err(|error| format!("{}: {error}", self.0.display()))
    }

    /// Removes it; one that is not there is no error.
    pub fn remove(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("{}: {error}", self.0.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn pending(from: &str, to: &str, starts: u32) -> Pending {
        Pending {
            id: "cmd".into(),
            from: from.into(),
            to: to.into(),
            starts,
            failed: None,
        }
    }

    #[test]
    fn limits_are_said_as_a_person_says_them() {
        for (secs, said) in [
            (0, "0 seconds"),
            (1, "1 second"),
            (59, "59 seconds"),
            (60, "1 minute"),
            (119, "1 minute"),
            (120, "2 minutes"),
            (50 * 60, "50 minutes"),
        ] {
            assert_eq!(spoken(Duration::from_secs(secs)), said);
        }
    }

    #[test]
    fn a_file_written_is_read_back_and_one_removed_twice_is_no_error() {
        let folder = tempfile::tempdir().expect("folder");
        let file = UpdateFile::in_state(folder.path());
        assert_eq!(file.read(), Ok(None));
        let written = pending("0.1.0+a", "0.1.0+b", 2);
        file.write(&written).expect("written");
        assert_eq!(file.read(), Ok(Some(written)));
        file.remove().expect("removed");
        file.remove().expect("removed again");
        assert_eq!(file.read(), Ok(None));
        std::fs::write(folder.path().join(UPDATE_FILE), "[]").expect("file");
        assert!(file.read().is_err());
        // Written before the field was there.
        std::fs::write(
            folder.path().join(UPDATE_FILE),
            r#"{"id":"c","from":"a","to":"b"}"#,
        )
        .expect("file");
        assert_eq!(
            file.read(),
            Ok(Some(Pending {
                id: "c".into(),
                from: "a".into(),
                to: "b".into(),
                starts: 0,
                failed: None,
            }))
        );
    }

    proptest! {
        /// The new program runs exactly `MAX_STARTS` times, then goes back; whatever else runs ends the update
        /// with the command's id.
        #[test]
        fn the_new_program_starts_at_most_max_starts_times(
            from in "0\\.1\\.0\\+[0-9a-f]{12}",
            to in "0\\.1\\.0\\+[0-9a-f]{12}",
            other in "0\\.1\\.0\\+[0-9a-f]{12}",
            starts in 0u32..10,
        ) {
            prop_assume!(from != to && other != to && other != from);
            let mut tries = 0;
            let mut found = Some(pending(&from, &to, starts));
            let went_back = loop {
                match at_start(found.take(), &to) {
                    AtStart::Trying(next) => {
                        prop_assert_eq!(next.starts, starts.max(tries) + 1);
                        tries = next.starts;
                        found = Some(next);
                    }
                    AtStart::GoBack(last) => break last,
                    other => prop_assert!(false, "{other:?}"),
                }
            };
            prop_assert_eq!(went_back.starts, MAX_STARTS.max(starts) + 1);
            prop_assert!(tries <= MAX_STARTS);
            match at_start(Some(went_back.clone()), &from) {
                AtStart::Failed { id, message } => {
                    prop_assert_eq!(id, "cmd");
                    prop_assert!(message.contains("taken back"), "{message}");
                }
                other => prop_assert!(false, "{other:?}"),
            }
            match at_start(Some(went_back), &other) {
                AtStart::Failed { id, message } => {
                    prop_assert_eq!(id, "cmd");
                    prop_assert!(message.contains("overtaken"), "{message}");
                }
                other => prop_assert!(false, "{other:?}"),
            }
            prop_assert_eq!(at_start(None, &to), AtStart::Nothing);
        }
    }
}
