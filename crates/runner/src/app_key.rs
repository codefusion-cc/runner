//! The GitHub App's key file a host held before the console held the key. A Runner on a settings file that still
//! names the key's folder (`app_dir`) signs with the key until the console's scale set calls answered this host
//! (`cf_daemon::HeldKey`); then it deletes the key file, takes `app_dir` out of the file and keeps everything else,
//! remembers where the file was, so that its report names a key file that is back or could not go, and starts again
//! through the console. A Runner that starts on a file whose key file its own take-out deleted takes the folder out at
//! once; a key file missing for another reason may come back, and its folder stays.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use cf_console_client::KEY_FILE_PATH_MAX as PATH_MAX;

use crate::config::{APP_DIR, Config};

/// The key file's name in the App's folder.
pub const KEY_FILE: &str = "app.pem";

/// The file in the state folder that names the key file a take-out of this Runner deleted, and on a second line the
/// file a link in its place named.
const REMEMBERED: &str = "app-key-file";

/// Held while a settings file is read, changed and put back, by the key's take-out and by the console's settings
/// (`ConfigFile::change`): each on a thread of its own, one would undo the other's change. A Runner started beside
/// it waits for the state folder's lock (`only_runner`) before it changes the file.
static CHANGING: Mutex<()> = Mutex::new(());

/// One change of the settings file at a time in this program: held from the read to the write.
pub fn one_change() -> MutexGuard<'static, ()> {
    CHANGING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What [`take_out`] did, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenOut {
    /// The key file it looked at; none when `app_dir` named no folder.
    pub key_file: Option<PathBuf>,
    /// What became of it.
    pub key: Key,
    /// Why the settings file still names the key's folder, when it does: the key file is missing and no take-out of
    /// this Runner deleted it, or the file could not be written.
    pub app_dir_kept: Option<String>,
}

/// When [`take_out`] takes the key out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// A call of the console's scale set calls this host made was answered: the key file goes.
    Answered,
    /// The Runner starts: only a key folder whose key file a take-out of this Runner deleted is taken out of the
    /// file, and nothing is deleted. A key file that is there stays and serves.
    KeyGone,
}

/// What became of the key file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// It was a file, and is deleted.
    Deleted,
    /// It was not there.
    Missing,
    /// It was a symbolic link: the link is deleted, and the file it named is left, as the Runner deletes nothing
    /// outside the App's folder.
    LinkDeleted { to: PathBuf },
    /// It could not be deleted; the report names it until it is gone.
    Kept { why: String },
}

/// Takes the App's key out of the host: when the settings file at `path` names `app_dir`, the key file in it is
/// remembered in the state folder and deleted, and the key is taken out of the file. `None` when the file names no
/// `app_dir`, or `when` is [`When::KeyGone`] and the key file is there: nothing is done, and nothing is read but the
/// file and the state folder.
///
/// [`When::KeyGone`] on a key file that is missing deletes nothing. It takes the folder out of the file only when
/// this Runner remembered that key file, so its own take-out deleted it; otherwise the folder stays and
/// [`TakenOut::app_dir_kept`] says why. A key file deleted and a file that could not be written are no failure:
/// the key is gone, and [`TakenOut::app_dir_kept`] says why the file still names its folder.
///
/// Refused, with nothing changed, when the file can be written by others, names no console, or the host holds no
/// token for it: the Runner could not get its GitHub tokens. Refused too when the key file is still there and the
/// file could not be written.
pub fn take_out(path: &Path, when: When) -> Result<Option<TakenOut>, String> {
    let _one = one_change();
    let said = |error: &dyn std::fmt::Display| format!("{}: {error}", path.display());
    let text = crate::config::read_settings(path)?;
    let mut file: serde_json::Value = serde_json::from_str(&text).map_err(|error| said(&error))?;
    let keys = file.as_object_mut().ok_or_else(|| said(&"no JSON object"))?;
    let Some(app_dir) = keys.get(APP_DIR).cloned() else {
        return Ok(None);
    };
    let console = keys.get("console").and_then(|console| console.as_str());
    let state_dir = keys
        .get("state_dir")
        .and_then(|state| state.as_str())
        .map(PathBuf::from)
        .filter(|state| state.is_absolute())
        .ok_or_else(|| said(&"state_dir: no absolute path"))?;
    let key_file = match &app_dir {
        serde_json::Value::Null => None,
        // A folder named through `..` could name a key file outside it; such a file is no Runner's.
        serde_json::Value::String(folder)
            if Path::new(folder).is_absolute()
                && !Path::new(folder)
                    .components()
                    .any(|part| part == Component::ParentDir) =>
        {
            Some(Path::new(folder).join(KEY_FILE))
        }
        _ => return Err(said(&"app_dir: no absolute path without `..`")),
    };
    if when == When::KeyGone
        && let Some(key_file) = &key_file
    {
        // A link counts as there, whatever it names: the key loads through it or the Runner says why it cannot.
        if std::fs::symlink_metadata(key_file).is_ok() {
            return Ok(None);
        }
        if !taken_out_here(&state_dir, key_file) {
            return Ok(Some(TakenOut {
                key_file: Some(key_file.clone()),
                key: Key::Missing,
                app_dir_kept: Some(NOT_TAKEN_OUT_HERE.into()),
            }));
        }
    }
    if console.is_none() {
        return Err(said(&KEPT_NO_CONSOLE));
    }
    if !enrolled(&state_dir) {
        return Err(said(&KEPT_NOT_ENROLLED));
    }

    keys.remove(APP_DIR);
    // A file a Runner could not start on changes nothing, the key included.
    let text = serde_json::to_string(&file).map_err(|error| said(&error))?;
    Config::parse(&text).map_err(|error| said(&error))?;

    // First what is remembered, then the key, then the file: a Runner that dies between two steps does the rest
    // when it starts again, and a key file left behind is always named. At a start the key file is gone already,
    // and nothing is deleted: a key file that came back meanwhile stays.
    let key = match &key_file {
        Some(_) if when == When::KeyGone => Key::Missing,
        Some(key_file) => {
            let linked = std::fs::read_link(key_file)
                .ok()
                .map(|to| match key_file.parent() {
                    Some(folder) if to.is_relative() => folder.join(to),
                    _ => to,
                });
            remember(&state_dir, key_file, linked.as_deref())?;
            // `unlink` never follows the last part of a path: a link is deleted and not the file it names.
            match std::fs::remove_file(key_file) {
                Ok(()) => match linked {
                    Some(to) => Key::LinkDeleted { to },
                    None => Key::Deleted,
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Key::Missing,
                Err(error) => Key::Kept {
                    why: error.to_string(),
                },
            }
        }
        None => Key::Missing,
    };
    let app_dir_kept = match write_settings(path, &file, |next| Config::load(next).map(|_| ())) {
        Ok(()) => None,
        // The key is still there: nothing is taken out, and it serves on.
        Err(why) if matches!(key, Key::Kept { .. }) => return Err(why),
        Err(why) => Some(why),
    };
    Ok(Some(TakenOut {
        key_file,
        key,
        app_dir_kept,
    }))
}

/// Why a Runner that starts keeps a key folder whose key file is missing.
pub const NOT_TAKEN_OUT_HERE: &str = "no take-out of this Runner deleted its key file, which may come back (a volume not mounted yet, a folder renamed)";

/// Why the key stays on a host whose file names no console.
pub const KEPT_NO_CONSOLE: &str =
    "the GitHub App's key stays: this file names no console (`console`) to give this host its GitHub tokens";
/// Why the key stays on a host that holds no token for its console.
pub const KEPT_NOT_ENROLLED: &str =
    "the GitHub App's key stays: this host holds no token for the console; run `enroll`";

/// Whether the host holds a token for its console, as `enroll` keeps it.
fn enrolled(state_dir: &Path) -> bool {
    std::fs::read_to_string(state_dir.join(crate::config::CONSOLE_TOKEN))
        .is_ok_and(|token| !token.trim().is_empty())
}

/// Keeps `key_file` in the state folder, for this user alone, and the file `linked` names when it is a link.
fn remember(state_dir: &Path, key_file: &Path, linked: Option<&Path>) -> Result<(), String> {
    let said = |error: std::io::Error| format!("{}: {error}", state_dir.display());
    cf_secret::private_dir(state_dir).map_err(said)?;
    let mut bytes = Vec::new();
    for path in std::iter::once(key_file).chain(linked) {
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.push(b'\n');
    }
    cf_secret::replace_private(&state_dir.join(REMEMBERED), &bytes).map_err(said)
}

/// What [`take_out`] remembered in `state_dir`: the key file it deleted, and the file a link in its place named.
/// Each an absolute path of at most [`PATH_MAX`] bytes.
fn remembered_files(state_dir: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let bytes = std::fs::read(state_dir.join(REMEMBERED)).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let mut lines = text.strip_suffix('\n').unwrap_or(text).split('\n');
    let path = |line: &str| {
        let path = PathBuf::from(line);
        (path.is_absolute() && line.len() <= PATH_MAX).then_some(path)
    };
    let key_file = path(lines.next()?)?;
    let linked = match lines.next() {
        Some(line) => Some(path(line)?),
        None => None,
    };
    lines.next().is_none().then_some((key_file, linked))
}

/// The key file the host held, as [`take_out`] remembered it in `state_dir` (the file a link in its place named): an
/// absolute path of at most [`PATH_MAX`] bytes. The report names it while something is there.
pub fn remembered(state_dir: &Path) -> Option<PathBuf> {
    remembered_files(state_dir).map(|(key_file, linked)| linked.unwrap_or(key_file))
}

/// Whether a take-out of the Runner of `state_dir` deleted `key_file`: it remembered it first.
fn taken_out_here(state_dir: &Path, key_file: &Path) -> bool {
    remembered_files(state_dir).is_some_and(|(remembered, _)| remembered == key_file)
}

/// Puts `file` in place of the settings file at `path` in one step, once `check` took it: it is written to a file of
/// its own beside it (`<path>.next`, for this user alone), checked there and renamed over it. Refused by `check`,
/// the settings file is left as it was and the error names it, not the file beside it.
pub fn write_settings(
    path: &Path,
    file: &serde_json::Value,
    check: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let said = |error: &dyn std::fmt::Display| format!("{}: {error}", path.display());
    let mut written = serde_json::to_string_pretty(file).map_err(|error| said(&error))?;
    written.push('\n');
    let next = path.with_extension("json.next");
    cf_secret::replace_private(&next, written.as_bytes()).map_err(|error| said(&error))?;
    if let Err(why) = check(&next) {
        let _ = std::fs::remove_file(&next);
        return Err(why.replace(&next.display().to_string(), &path.display().to_string()));
    }
    std::fs::rename(&next, path).map_err(|error| {
        let _ = std::fs::remove_file(&next);
        said(&error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// A host as an update finds it: a settings file that names the App's folder with its key, a console and its
    /// token.
    struct Host {
        folder: tempfile::TempDir,
    }

    const KEY: &str = "-----BEGIN RSA PRIVATE KEY-----\nnot a key, a marker\n-----END RSA PRIVATE KEY-----\n";

    impl Host {
        fn new() -> Host {
            let host = Host {
                folder: tempfile::tempdir().expect("folder"),
            };
            std::fs::create_dir_all(host.app()).expect("the App's folder");
            std::fs::write(host.app().join("app.json"), r#"{"client_id":"Iv23abc"}"#).expect("the App");
            std::fs::write(host.key(), KEY).expect("the key");
            std::fs::create_dir_all(host.state()).expect("the state folder");
            std::fs::write(host.state().join("console-token"), "cfr_token\n").expect("a token");
            host.write(&host.settings(true));
            host
        }

        fn app(&self) -> PathBuf {
            self.folder.path().join("app")
        }

        fn key(&self) -> PathBuf {
            self.app().join(KEY_FILE)
        }

        fn state(&self) -> PathBuf {
            self.folder.path().join("state")
        }

        fn path(&self) -> PathBuf {
            self.folder.path().join("config.json")
        }

        fn settings(&self, console: bool) -> serde_json::Value {
            let mut settings = serde_json::json!({
                "machine": "mac-a-vm",
                "state_dir": self.state(),
                "app_dir": self.app(),
                "image": "2026.10.1",
                "use": "dedicated",
                "max_jobs": 2,
                "repos": ["org/app"],
            });
            if console {
                settings["console"] = "https://console.example".into();
            }
            settings
        }

        fn write(&self, settings: &serde_json::Value) {
            std::fs::write(self.path(), settings.to_string()).expect("written");
            std::fs::set_permissions(self.path(), std::fs::Permissions::from_mode(0o600)).expect("a mode");
        }

        fn read(&self) -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(self.path()).expect("read")).expect("JSON")
        }
    }

    #[test]
    fn the_key_file_is_deleted_app_dir_taken_out_and_every_other_key_kept() {
        let host = Host::new();
        let mut expected = host.settings(true);
        expected.as_object_mut().expect("object").remove("app_dir");
        let taken = take_out(&host.path(), When::Answered)
            .expect("taken out")
            .expect("there was one");
        assert_eq!(
            taken,
            TakenOut {
                key_file: Some(host.key()),
                key: Key::Deleted,
                app_dir_kept: None,
            }
        );
        assert!(std::fs::symlink_metadata(host.key()).is_err(), "the key is gone");
        assert!(host.app().join("app.json").exists(), "only the key file goes");
        assert_eq!(host.read(), expected);
        assert_eq!(
            std::fs::metadata(host.path())
                .expect("there")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!host.path().with_extension("json.next").exists());
        Config::load(&host.path()).expect("a Runner starts on it");
        assert_eq!(remembered(&host.state()), Some(host.key()));
        assert_eq!(
            std::fs::metadata(host.state().join(REMEMBERED))
                .expect("there")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "for this user alone"
        );
    }

    #[test]
    fn taken_out_twice_is_taken_out_once() {
        let host = Host::new();
        take_out(&host.path(), When::Answered)
            .expect("first")
            .expect("there was one");
        let after = std::fs::read(host.path()).expect("read");
        assert_eq!(
            take_out(&host.path(), When::Answered),
            Ok(None),
            "nothing left to do"
        );
        assert_eq!(
            std::fs::read(host.path()).expect("read"),
            after,
            "not written again"
        );
        assert_eq!(remembered(&host.state()), Some(host.key()));
    }

    #[test]
    fn a_runner_that_died_after_the_key_went_does_the_rest() {
        let host = Host::new();
        std::fs::remove_file(host.key()).expect("gone already");
        let taken = take_out(&host.path(), When::Answered)
            .expect("taken out")
            .expect("there was one");
        assert_eq!(taken.key, Key::Missing);
        assert!(host.read().get("app_dir").is_none());
        // A folder that was named null holds no key.
        let host = Host::new();
        let mut settings = host.settings(true);
        settings["app_dir"] = serde_json::Value::Null;
        host.write(&settings);
        assert_eq!(
            take_out(&host.path(), When::Answered)
                .expect("taken out")
                .expect("named")
                .key,
            Key::Missing
        );
        assert!(host.key().exists(), "a key no folder names is not looked for");
        assert!(host.read().get("app_dir").is_none());
    }

    #[test]
    fn without_a_console_or_its_token_nothing_changes_and_the_runner_does_not_start() {
        let host = Host::new();
        host.write(&host.settings(false));
        let before = std::fs::read(host.path()).expect("read");
        let said = take_out(&host.path(), When::Answered).expect_err("refused");
        assert_eq!(said, format!("{}: {KEPT_NO_CONSOLE}", host.path().display()));
        assert_eq!(std::fs::read_to_string(host.key()).expect("kept"), KEY);
        assert_eq!(std::fs::read(host.path()).expect("read"), before);
        assert_eq!(remembered(&host.state()), None);

        let host = Host::new();
        std::fs::remove_file(host.state().join("console-token")).expect("not enrolled");
        let said = take_out(&host.path(), When::Answered).expect_err("refused");
        assert_eq!(said, format!("{}: {KEPT_NOT_ENROLLED}", host.path().display()));
        assert_eq!(std::fs::read_to_string(host.key()).expect("kept"), KEY);
        assert!(host.read().get("app_dir").is_some());

        std::fs::write(host.state().join("console-token"), " \n").expect("an empty token");
        assert!(
            take_out(&host.path(), When::Answered).is_err(),
            "an empty token is none"
        );
    }

    #[test]
    fn a_runner_that_starts_keeps_a_key_file_that_is_there() {
        // The key file there: nothing changes, whether or not the host could go through the console.
        for console in [true, false] {
            let host = Host::new();
            host.write(&host.settings(console));
            let before = std::fs::read(host.path()).expect("read");
            assert_eq!(take_out(&host.path(), When::KeyGone), Ok(None), "{console}");
            assert_eq!(std::fs::read_to_string(host.key()).expect("kept"), KEY);
            assert_eq!(std::fs::read(host.path()).expect("read"), before);
            assert_eq!(remembered(&host.state()), None);
        }
        // A link counts as there, even one that names nothing: the Runner says why the key does not load.
        let host = Host::new();
        std::fs::remove_file(host.key()).expect("removed");
        std::os::unix::fs::symlink("nowhere.pem", host.key()).expect("a link");
        assert_eq!(take_out(&host.path(), When::KeyGone), Ok(None));
        assert!(host.read().get("app_dir").is_some());
    }

    /// A Runner that starts on a file whose key file is missing takes the folder out only when its own take-out
    /// deleted that file. Missing otherwise (a volume not mounted yet, a folder renamed, the take-out of another
    /// Runner that shares the folder), the key may come back: the folder stays, and nothing is deleted.
    #[test]
    fn a_start_takes_a_key_folder_out_only_after_its_own_take_out_deleted_the_key() {
        let host = Host::new();
        std::fs::remove_file(host.key()).expect("missing");
        let before = std::fs::read(host.path()).expect("read");
        let taken = take_out(&host.path(), When::KeyGone)
            .expect("the Runner goes on")
            .expect("said");
        assert_eq!(
            taken,
            TakenOut {
                key_file: Some(host.key()),
                key: Key::Missing,
                app_dir_kept: Some(NOT_TAKEN_OUT_HERE.into()),
            }
        );
        assert_eq!(std::fs::read(host.path()).expect("read"), before, "app_dir stays");
        assert_eq!(remembered(&host.state()), None);
        // Back again: it serves.
        std::fs::write(host.key(), KEY).expect("mounted");
        assert_eq!(take_out(&host.path(), When::KeyGone), Ok(None));
        assert_eq!(std::fs::read_to_string(host.key()).expect("kept"), KEY);

        // What this Runner took out was another key file.
        std::fs::remove_file(host.key()).expect("missing");
        std::fs::write(host.state().join(REMEMBERED), "/elsewhere/app.pem\n").expect("remembered");
        let taken = take_out(&host.path(), When::KeyGone)
            .expect("the Runner goes on")
            .expect("said");
        assert_eq!(taken.app_dir_kept.as_deref(), Some(NOT_TAKEN_OUT_HERE));
        assert_eq!(std::fs::read(host.path()).expect("read"), before, "app_dir stays");
        // A link it deleted is remembered with the file it named.
        std::fs::write(
            host.state().join(REMEMBERED),
            format!("{}\n/elsewhere/app.pem\n", host.key().display()),
        )
        .expect("remembered");
        let host_without_console = Host::new();
        host_without_console.write(&host_without_console.settings(false));
        std::fs::remove_file(host_without_console.key()).expect("missing");
        let before_without = std::fs::read(host_without_console.path()).expect("read");
        assert_eq!(
            take_out(&host_without_console.path(), When::KeyGone)
                .expect("the Runner goes on")
                .expect("said")
                .app_dir_kept
                .as_deref(),
            Some(NOT_TAKEN_OUT_HERE),
            "nothing to take out, whatever the file names"
        );
        assert_eq!(
            std::fs::read(host_without_console.path()).expect("read"),
            before_without
        );
        let taken = take_out(&host.path(), When::KeyGone)
            .expect("taken out")
            .expect("there was one");
        assert_eq!(taken.app_dir_kept, None);
        assert!(host.read().get("app_dir").is_none());
        host.write(&host.settings(true));

        // Its own take-out remembered the file and deleted it, and the Runner ended before the file was written.
        std::fs::write(
            host.state().join(REMEMBERED),
            format!("{}\n", host.key().display()),
        )
        .expect("remembered");
        let taken = take_out(&host.path(), When::KeyGone)
            .expect("taken out")
            .expect("there was one");
        assert_eq!(taken.key, Key::Missing);
        assert!(host.read().get("app_dir").is_none());
        Config::load(&host.path()).expect("a Runner starts on it");
        // Without a console to go through: refused, and nothing changes.
        host.write(&host.settings(false));
        let said = take_out(&host.path(), When::KeyGone).expect_err("refused");
        assert!(said.ends_with(KEPT_NO_CONSOLE), "{said}");
        assert!(host.read().get("app_dir").is_some());
    }

    /// The key went and the settings file could not be written: the take-out says so, the key gone and the file
    /// still naming its folder, and is no failure; a Runner that starts on that file goes on, and takes the folder
    /// out once the file can be written.
    #[test]
    fn a_take_out_whose_file_could_not_be_written_says_the_key_is_gone_and_the_next_start_goes_on() {
        let host = Host::new();
        // Nothing new can be written beside the settings file, the next file of a change included.
        std::fs::set_permissions(host.folder.path(), std::fs::Permissions::from_mode(0o500)).expect("a mode");
        let taken = take_out(&host.path(), When::Answered);
        let started = take_out(&host.path(), When::KeyGone);
        std::fs::set_permissions(host.folder.path(), std::fs::Permissions::from_mode(0o700)).expect("a mode");
        let taken = taken.expect("the key is out").expect("there was one");
        assert_eq!(taken.key, Key::Deleted);
        assert!(
            taken
                .app_dir_kept
                .as_deref()
                .is_some_and(|why| why.contains("config.json")),
            "{taken:?}"
        );
        assert!(std::fs::symlink_metadata(host.key()).is_err(), "the key is gone");
        assert!(host.read().get("app_dir").is_some(), "the file still names it");
        assert_eq!(remembered(&host.state()), Some(host.key()));
        let started = started.expect("a start goes on").expect("said");
        assert_eq!(started.key, Key::Missing);
        assert!(started.app_dir_kept.is_some(), "{started:?}");
        assert!(host.read().get("app_dir").is_some());
        // A key that could not be deleted either is not taken out at all: it serves on.
        let kept = Host::new();
        std::fs::remove_file(kept.key()).expect("removed");
        std::fs::create_dir_all(kept.key().join("inside")).expect("a folder in its place");
        std::fs::set_permissions(kept.folder.path(), std::fs::Permissions::from_mode(0o500)).expect("a mode");
        let refused = take_out(&kept.path(), When::Answered);
        std::fs::set_permissions(kept.folder.path(), std::fs::Permissions::from_mode(0o700)).expect("a mode");
        assert!(refused.is_err(), "{refused:?}");
        assert!(kept.key().join("inside").exists());

        let taken = take_out(&host.path(), When::KeyGone)
            .expect("taken out")
            .expect("there was one");
        assert_eq!(taken.key, Key::Missing);
        assert!(host.read().get("app_dir").is_none());
    }

    #[test]
    fn a_settings_file_others_can_write_is_not_acted_on() {
        let host = Host::new();
        std::fs::set_permissions(host.path(), std::fs::Permissions::from_mode(0o666)).expect("a mode");
        assert!(
            take_out(&host.path(), When::Answered)
                .expect_err("refused")
                .contains("others can write it")
        );
        assert_eq!(std::fs::read_to_string(host.key()).expect("kept"), KEY);
    }

    #[test]
    fn only_a_file_named_app_pem_in_the_named_folder_is_ever_deleted() {
        // A link in its place: the link goes, and what it names stays and is remembered for the report.
        let host = Host::new();
        let elsewhere = host.folder.path().join("elsewhere.pem");
        std::fs::write(&elsewhere, KEY).expect("a key elsewhere");
        std::fs::remove_file(host.key()).expect("removed");
        std::os::unix::fs::symlink("../elsewhere.pem", host.key()).expect("a link");
        let taken = take_out(&host.path(), When::Answered)
            .expect("taken out")
            .expect("there was one");
        let to = host.app().join("../elsewhere.pem");
        assert_eq!(taken.key, Key::LinkDeleted { to: to.clone() });
        assert!(std::fs::symlink_metadata(host.key()).is_err(), "the link is gone");
        assert_eq!(std::fs::read_to_string(&elsewhere).expect("kept"), KEY);
        assert_eq!(remembered(&host.state()), Some(to));
        assert!(
            taken_out_here(&host.state(), &host.key()),
            "a start after it knows"
        );

        // A folder in its place is not emptied: it stays, and is named.
        let host = Host::new();
        std::fs::remove_file(host.key()).expect("removed");
        std::fs::create_dir_all(host.key().join("inside")).expect("a folder");
        let taken = take_out(&host.path(), When::Answered)
            .expect("taken out")
            .expect("there was one");
        assert!(matches!(taken.key, Key::Kept { .. }), "{taken:?}");
        assert!(host.key().join("inside").exists());
        assert!(
            host.read().get("app_dir").is_none(),
            "the Runner goes on without the key"
        );
        assert_eq!(remembered(&host.state()), Some(host.key()));

        // A folder that is no absolute path, or that goes through `..`, is refused.
        for app_dir in [
            serde_json::json!("app"),
            serde_json::json!(""),
            serde_json::json!(7),
        ] {
            let host = Host::new();
            let mut settings = host.settings(true);
            settings["app_dir"] = app_dir.clone();
            host.write(&settings);
            assert!(
                take_out(&host.path(), When::Answered)
                    .expect_err("refused")
                    .contains("no absolute path"),
                "{app_dir}"
            );
            assert_eq!(std::fs::read_to_string(host.key()).expect("kept"), KEY);
        }
        let host = Host::new();
        let mut settings = host.settings(true);
        let app = host.app();
        let around = app.join("..").join(app.file_name().expect("a name"));
        settings["app_dir"] = serde_json::json!(around);
        host.write(&settings);
        assert!(
            take_out(&host.path(), When::Answered)
                .expect_err("refused")
                .contains("`..`"),
            "a folder named through `..`"
        );
        assert_eq!(std::fs::read_to_string(host.key()).expect("kept"), KEY);
        assert_eq!(host.read()["app_dir"], serde_json::json!(around));
    }

    #[test]
    fn a_file_that_would_not_start_a_runner_is_left_as_it_was() {
        let host = Host::new();
        let mut settings = host.settings(true);
        settings["max_jobs"] = 0.into();
        host.write(&settings);
        let said = take_out(&host.path(), When::Answered).expect_err("refused");
        assert!(said.contains("max_jobs") && !said.contains("json.next"), "{said}");
        assert!(host.read().get("app_dir").is_some());
        assert_eq!(std::fs::read_to_string(host.key()).expect("kept"), KEY);
        assert_eq!(remembered(&host.state()), None);
        assert!(!host.path().with_extension("json.next").exists());
    }

    #[test]
    fn only_a_remembered_absolute_path_is_named() {
        let state = tempfile::tempdir().expect("folder");
        assert_eq!(remembered(state.path()), None, "nothing remembered");
        for (text, named) in [
            ("/Users/someone/app/app.pem\n", true),
            ("app/app.pem\n", false),
            ("", false),
            ("/a\nb\n", false),
            ("/a\n/b\n/c\n", false),
            ("/a\n\n", false),
        ] {
            std::fs::write(state.path().join(REMEMBERED), text).expect("written");
            assert_eq!(remembered(state.path()).is_some(), named, "{text:?}");
        }
        // A link's: the file it named.
        std::fs::write(state.path().join(REMEMBERED), "/a/app.pem\n/b.pem\n").expect("written");
        assert_eq!(remembered(state.path()), Some(PathBuf::from("/b.pem")));
        assert!(taken_out_here(state.path(), Path::new("/a/app.pem")));
        assert!(!taken_out_here(state.path(), Path::new("/b.pem")));
        std::fs::write(
            state.path().join(REMEMBERED),
            format!("/{}\n", "a".repeat(PATH_MAX)),
        )
        .expect("written");
        assert_eq!(remembered(state.path()), None, "longer than the report carries");
        std::fs::write(
            state.path().join(REMEMBERED),
            format!("/{}\n", "a".repeat(PATH_MAX - 1)),
        )
        .expect("written");
        assert!(
            remembered(state.path()).is_some(),
            "as long as the report carries"
        );
    }
}
