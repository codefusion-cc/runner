//! What a Linux host needs before the Runner can start machines on it, as lines of `check`: each says what is there,
//! or what is missing with the one command, run as root, that brings it.

use std::path::{Path, PathBuf};

/// What `check` finds on a Linux host. Read by [`Facts::of_this_host`]; a test writes its own.
#[derive(Debug, Clone)]
pub struct Facts {
    /// The user the Runner runs as, as the user database names it.
    pub user: String,
    /// That user's number.
    pub uid: u32,
    /// Whether `/dev/kvm` opens for reading and writing, and why not.
    pub kvm: Result<(), Kvm>,
    /// Whether the host uses the unified cgroup hierarchy (cgroup v2).
    pub cgroup2: bool,
    /// The controllers the user's systemd may hand on (`cgroup.controllers` of `user@<uid>.service`); `None` while
    /// no systemd of the user's runs.
    pub delegated: Option<String>,
    /// Whether lingering is on for the user: their systemd runs from the host's start, not only while they are
    /// logged in.
    pub linger: bool,
    /// pasta, where it is, or why it cannot be used.
    pub pasta: Result<PathBuf, String>,
    /// Whether an unprivileged process may make a user namespace and map itself in it, and why not.
    pub userns: Result<(), String>,
    /// The kernel's switches that keep unprivileged user namespaces from a user.
    pub userns_switches: Switches,
    /// The file system of the state folder (or of the nearest folder above it that is there), when it is mounted
    /// `noexec`: where it is mounted.
    pub noexec: Option<PathBuf>,
}

/// Why `/dev/kvm` does not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kvm {
    /// The kernel offers no KVM.
    Missing,
    /// The user may not open it.
    Denied,
    /// Another error.
    Other(String),
}

/// The kernel's switches for unprivileged user namespaces, as far as this kernel has them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Switches {
    /// `kernel.apparmor_restrict_unprivileged_userns` (Ubuntu 23.10 and later): 1 holds them to what AppArmor allows.
    pub apparmor_restrict: Option<u64>,
    /// `kernel.unprivileged_userns_clone` (Debian's kernels before 6.1 and some others): 0 allows them to root only.
    pub unprivileged_clone: Option<u64>,
    /// `user.max_user_namespaces`: 0 allows none.
    pub max_user_namespaces: Option<u64>,
}

impl Facts {
    /// Reads this host, `root` standing for `/` (a folder of files in a test), for the state folder `state_dir`.
    pub fn of_this_host(root: &Path, state_dir: &Path) -> Facts {
        let (uid, user) = cf_daemon::platform::this_user();
        let read = |path: &str| std::fs::read_to_string(under(root, path)).ok();
        let number = |path: &str| read(path).and_then(|text| text.trim().parse().ok());
        Facts {
            kvm: open_kvm(&under(root, "/dev/kvm")),
            cgroup2: under(root, "/sys/fs/cgroup/cgroup.controllers").exists(),
            delegated: read(&format!(
                "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers"
            )),
            linger: under(root, &format!("/var/lib/systemd/linger/{user}")).exists(),
            pasta: cf_vmm_ch::programs::pasta(),
            userns: try_user_namespace(),
            userns_switches: Switches {
                apparmor_restrict: number("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"),
                unprivileged_clone: number("/proc/sys/kernel/unprivileged_userns_clone"),
                max_user_namespaces: number("/proc/sys/user/max_user_namespaces"),
            },
            noexec: noexec_mount(state_dir),
            user,
            uid,
        }
    }
}

/// `path` (absolute, as on a real host) under `root`.
fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

fn open_kvm(path: &Path) -> Result<(), Kvm> {
    match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Kvm::Missing),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(Kvm::Denied),
        Err(error) => Err(Kvm::Other(error.to_string())),
    }
}

/// Starts `/bin/true` in a user namespace of its own with this user mapped in it, as pasta does for a machine.
fn try_user_namespace() -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: no arguments; they cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let (uid_map, gid_map) = (format!("0 {uid} 1"), format!("0 {gid} 1"));
    let write = |path: &std::ffi::CStr, text: &[u8]| -> std::io::Result<()> {
        // SAFETY: the path ends with a NUL; the descriptor is this call's own and closed before it returns.
        unsafe {
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let wrote = libc::write(fd, text.as_ptr().cast(), text.len());
            let error = std::io::Error::last_os_error();
            libc::close(fd);
            if usize::try_from(wrote).ok() != Some(text.len()) {
                return Err(error);
            }
        }
        Ok(())
    };
    let mut command = std::process::Command::new("/bin/true");
    // SAFETY: between fork and exec the child only makes system calls (unshare, open, write, close) on what was
    // made before the fork, and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            write(c"/proc/self/setgroups", b"deny")?;
            write(c"/proc/self/uid_map", uid_map.as_bytes())?;
            write(c"/proc/self/gid_map", gid_map.as_bytes())
        });
    }
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("/bin/true in a user namespace: {status}")),
        Err(error) => Err(error.to_string()),
    }
}

/// Where the file system of `folder`, or of the nearest folder above it that is there, is mounted, when it is mounted
/// `noexec`: cloud-hypervisor is unpacked into the state folder and run from there.
fn noexec_mount(folder: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    let there = folder.ancestors().find(|each| each.exists())?;
    let path = std::ffi::CString::new(there.as_os_str().as_encoded_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: the path ends with a NUL and `stats` is a statvfs this call may write.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so it filled the struct in.
    let flags = unsafe { stats.assume_init() }.f_flag;
    if flags & libc::ST_NOEXEC == 0 {
        return None;
    }
    // The mount point: the highest folder above that is on the same device.
    let device = std::fs::metadata(there).ok()?.dev();
    let mount = there
        .ancestors()
        .take_while(|each| std::fs::metadata(each).is_ok_and(|found| found.dev() == device))
        .last()
        .unwrap_or(there);
    Some(mount.to_path_buf())
}

/// The lines of `check` for what a Linux host needs, in the order they are best fixed in.
pub fn lines(facts: &Facts) -> Vec<Result<String, String>> {
    let Facts { user, uid, .. } = facts;
    let restart_manager = format!("sudo systemctl restart user@{uid}.service");
    let mut lines = vec![match &facts.kvm {
        Ok(()) => Ok("/dev/kvm opens for reading and writing".to_string()),
        Err(Kvm::Missing) => Err(
            "/dev/kvm is not there: this kernel or board offers no KVM, and no machine can start here"
                .to_string(),
        ),
        Err(Kvm::Denied) => Err(format!(
            "/dev/kvm does not open for {user}; as root: `sudo usermod -aG kvm {user} && {restart_manager}`"
        )),
        Err(Kvm::Other(error)) => Err(format!("/dev/kvm: {error}")),
    }];
    lines.push(if facts.linger {
        Ok(format!(
            "lingering is on: the systemd of {user} runs from the host's start"
        ))
    } else {
        Err(format!(
            "lingering is off: the Runner would run only while {user} is logged in; as root: `sudo loginctl \
             enable-linger {user}`"
        ))
    });
    let delegate = format!(
        "sudo mkdir -p /etc/systemd/system/user@.service.d && printf '[Service]\\nDelegate=cpu cpuset io memory \
         pids\\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf && sudo systemctl daemon-reload && \
         {restart_manager}"
    );
    lines.push(match (&facts.cgroup2, &facts.delegated) {
        (false, _) => Err(
            "this host does not use cgroup v2, which machines need: boot it with `systemd.unified_cgroup_hierarchy=1` \
             on the kernel's command line"
                .to_string(),
        ),
        (true, None) => Err(format!(
            "what the systemd of {user} may hand on is not known while it does not run: turn lingering on first"
        )),
        (true, Some(controllers)) => {
            let has = |name: &str| controllers.split_whitespace().any(|each| each == name);
            if has("memory") && has("pids") {
                Ok(format!(
                    "cgroup v2, with memory and pids handed on to {user}'s machines"
                ))
            } else {
                Err(format!(
                    "the systemd of {user} is not handed the memory and pids controllers (it has: {}); as root: \
                     `{delegate}`",
                    controllers.trim()
                ))
            }
        }
    });
    lines.push(match &facts.userns {
        Ok(()) => Ok("unprivileged user namespaces are allowed".to_string()),
        Err(error) => {
            let Switches {
                apparmor_restrict,
                unprivileged_clone,
                max_user_namespaces,
            } = facts.userns_switches;
            let set = |setting: &str| {
                format!(
                    "; as root: `echo {setting} | sudo tee /etc/sysctl.d/60-codefusion-runner.conf && sudo sysctl \
                     --system`"
                )
            };
            let fix = if apparmor_restrict == Some(1) {
                set("kernel.apparmor_restrict_unprivileged_userns=0")
            } else if unprivileged_clone == Some(0) {
                set("kernel.unprivileged_userns_clone=1")
            } else if max_user_namespaces == Some(0) {
                set("user.max_user_namespaces=15000")
            } else {
                String::new()
            };
            Err(format!(
                "no user namespace for {user} ({error}), which every machine's network and sandbox need{fix}"
            ))
        }
    });
    lines.push(match &facts.pasta {
        Ok(path) => Ok(format!("pasta at {}", path.display())),
        Err(error) => Err(format!(
            "pasta cannot be used: {error}; as root: `sudo apt install passt`"
        )),
    });
    lines.push(match &facts.noexec {
        None => Ok("the state folder's file system runs programs".to_string()),
        Some(mount) => Err(format!(
            "the state folder is on {}, mounted noexec, and cloud-hypervisor runs from it: give `state_dir` a \
             folder elsewhere, or as root: `sudo mount -o remount,exec {}` (and leave noexec out of /etc/fstab)",
            mount.display(),
            mount.display()
        )),
    });
    lines
}

/// `codefusion-runner vmm`: the virtual machine monitor this build carries; an error when it carries none. What an
/// install and an update ask of a program before it is put in place.
pub fn vmm() -> Result<(), String> {
    if cf_vmm_ch::programs::embedded().is_none() {
        return Err(
            "this build carries no cloud-hypervisor: build it with `cargo xtask build` on Linux for arm64"
                .into(),
        );
    }
    println!("cloud-hypervisor {}", cf_vmm_ch::pins::CLOUD_HYPERVISOR_VERSION);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that has everything.
    fn ready() -> Facts {
        Facts {
            user: "codefusion-runner".into(),
            uid: 1002,
            kvm: Ok(()),
            cgroup2: true,
            delegated: Some("cpu memory pids\n".into()),
            linger: true,
            pasta: Ok("/usr/bin/pasta".into()),
            userns: Ok(()),
            userns_switches: Switches::default(),
            noexec: None,
        }
    }

    fn missing(facts: &Facts) -> Vec<String> {
        lines(facts).into_iter().filter_map(Result::err).collect()
    }

    #[test]
    fn a_ready_host_misses_nothing() {
        let lines = lines(&ready());
        assert_eq!(lines.len(), 6);
        assert!(lines.iter().all(Result::is_ok), "{lines:?}");
    }

    #[test]
    fn each_missing_thing_is_one_line_with_the_root_command_that_brings_it() {
        let restart = "sudo systemctl restart user@1002.service";
        let cases: Vec<(Facts, String)> = vec![
            (
                Facts {
                    kvm: Err(Kvm::Denied),
                    ..ready()
                },
                format!("`sudo usermod -aG kvm codefusion-runner && {restart}`"),
            ),
            (
                Facts {
                    linger: false,
                    ..ready()
                },
                "`sudo loginctl enable-linger codefusion-runner`".into(),
            ),
            (
                Facts {
                    delegated: Some("cpu\n".into()),
                    ..ready()
                },
                format!(
                    "(it has: cpu); as root: `sudo mkdir -p /etc/systemd/system/user@.service.d && printf \
                     '[Service]\\nDelegate=cpu cpuset io memory pids\\n' | sudo tee \
                     /etc/systemd/system/user@.service.d/delegate.conf && sudo systemctl daemon-reload && {restart}`"
                ),
            ),
            (
                Facts {
                    pasta: Err(
                        "pasta is not installed (`apt install passt`): machines with a network need it"
                            .into(),
                    ),
                    ..ready()
                },
                "; as root: `sudo apt install passt`".into(),
            ),
            (
                Facts {
                    userns: Err("Operation not permitted (os error 1)".into()),
                    userns_switches: Switches {
                        apparmor_restrict: Some(1),
                        max_user_namespaces: Some(30000),
                        ..Switches::default()
                    },
                    ..ready()
                },
                "`echo kernel.apparmor_restrict_unprivileged_userns=0 | sudo tee \
                 /etc/sysctl.d/60-codefusion-runner.conf && sudo sysctl --system`"
                    .into(),
            ),
            (
                Facts {
                    userns: Err("Operation not permitted (os error 1)".into()),
                    userns_switches: Switches {
                        unprivileged_clone: Some(0),
                        ..Switches::default()
                    },
                    ..ready()
                },
                "`echo kernel.unprivileged_userns_clone=1 | sudo tee".into(),
            ),
            (
                Facts {
                    userns: Err("No space left on device (os error 28)".into()),
                    userns_switches: Switches {
                        max_user_namespaces: Some(0),
                        ..Switches::default()
                    },
                    ..ready()
                },
                "`echo user.max_user_namespaces=15000 | sudo tee".into(),
            ),
            (
                Facts {
                    noexec: Some("/data".into()),
                    ..ready()
                },
                "`sudo mount -o remount,exec /data`".into(),
            ),
        ];
        for (facts, says) in cases {
            let missing = missing(&facts);
            assert_eq!(missing.len(), 1, "{missing:?}");
            assert!(
                missing[0].contains(&says),
                "{:?} does not say {says:?}",
                missing[0]
            );
        }
    }

    #[test]
    fn what_has_no_root_command_says_why_and_names_none() {
        let no_kvm = missing(&Facts {
            kvm: Err(Kvm::Missing),
            ..ready()
        });
        assert_eq!(
            no_kvm,
            ["/dev/kvm is not there: this kernel or board offers no KVM, and no machine can start here"]
        );
        let v1 = missing(&Facts {
            cgroup2: false,
            ..ready()
        });
        assert!(v1[0].contains("systemd.unified_cgroup_hierarchy=1"), "{v1:?}");
        // Without a manager of the user's, what it may hand on cannot be read: lingering comes first.
        let off = missing(&Facts {
            linger: false,
            delegated: None,
            ..ready()
        });
        assert_eq!(off.len(), 2, "{off:?}");
        assert!(off[1].contains("turn lingering on first"), "{off:?}");
        // A kernel that refuses for a reason none of its switches names gets no command that would not help.
        let userns = missing(&Facts {
            userns: Err("Operation not permitted (os error 1)".into()),
            ..ready()
        });
        assert!(!userns[0].contains("sudo"), "{userns:?}");
    }

    #[test]
    fn controllers_are_whole_words() {
        let missing = missing(&Facts {
            delegated: Some("cpu memory_x pidsx\n".into()),
            ..ready()
        });
        assert_eq!(missing.len(), 1, "{missing:?}");
    }

    #[test]
    fn the_facts_are_read_from_the_hosts_files() {
        let root = tempfile::tempdir().expect("root");
        let write = |path: &str, text: &str| {
            let path = under(root.path(), path);
            std::fs::create_dir_all(path.parent().expect("a folder")).expect("folder");
            std::fs::write(path, text).expect("a file");
        };
        let (uid, user) = cf_daemon::platform::this_user();
        let empty = Facts::of_this_host(root.path(), root.path());
        assert!(!empty.cgroup2 && !empty.linger && empty.delegated.is_none());
        assert_eq!(empty.kvm, Err(Kvm::Missing));
        assert_eq!(empty.userns_switches, Switches::default());
        write("/sys/fs/cgroup/cgroup.controllers", "cpu memory pids\n");
        write(
            &format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers"),
            "memory pids\n",
        );
        write(&format!("/var/lib/systemd/linger/{user}"), "");
        write("/proc/sys/kernel/apparmor_restrict_unprivileged_userns", "1\n");
        write("/proc/sys/user/max_user_namespaces", "63223\n");
        write("/dev/kvm", "");
        let facts = Facts::of_this_host(root.path(), root.path());
        assert!(facts.cgroup2 && facts.linger);
        assert_eq!(facts.delegated.as_deref(), Some("memory pids\n"));
        assert_eq!(facts.kvm, Ok(()));
        assert_eq!(
            facts.userns_switches,
            Switches {
                apparmor_restrict: Some(1),
                unprivileged_clone: None,
                max_user_namespaces: Some(63223),
            }
        );
        // A state folder that is not made yet is judged by the folder above it.
        assert_eq!(noexec_mount(&root.path().join("not/there/yet")), None);
    }
}
