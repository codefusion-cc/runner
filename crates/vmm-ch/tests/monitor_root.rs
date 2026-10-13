//! What the monitor sees of the host, as the sandbox gives it: this program is the monitor, run by the real
//! sandbox ([`cf_vmm_ch::sandbox::main`]) without a network, so no pasta and no KVM are needed. It is shown a file to
//! read, one to write, a folder, a copy of itself to read, two devices and its own folder, and tries what a monitor
//! taken over by its guest would try. Each check below says what it must find.
//!
//! It runs without a test harness: the sandbox makes namespaces only a process of one thread may make. It needs user
//! namespaces.

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
}

#[cfg(target_os = "linux")]
mod linux {
    use cf_vmm::Network;
    use cf_vmm_ch::sandbox::{Access, ROOT, SUBCOMMAND, Sandbox};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Read as _};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Set for the sandbox and the monitor this program runs, so a plain run of it never takes their roles.
    const MARK: &str = "CF_MONITOR_ROOT_TEST";
    /// The monitor's subcommand.
    const PROBES: &str = "monitor-probes";
    /// A descriptor of the host's root the sandbox is handed open, as a careless parent might.
    const LEAKED: i32 = 100;

    pub fn main() {
        let args: Vec<OsString> = std::env::args_os().collect();
        let marked = std::env::var_os(MARK).is_some();
        match args.get(1).and_then(|arg| arg.to_str()) {
            Some(SUBCOMMAND) if marked => {
                // SAFETY: open reads a NUL terminated literal; dup2 takes no pointers. Neither sets close-on-exec.
                unsafe {
                    let fd = libc::open(c"/".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
                    assert!(fd >= 0 && libc::dup2(fd, LEAKED) == LEAKED);
                }
                std::process::exit(cf_vmm_ch::sandbox::main(&args[2..]));
            }
            Some(PROBES) if marked => {
                probes(&args[2..]);
                std::process::exit(0);
            }
            _ => check(),
        }
    }

    /// The paths of a run, on the host.
    struct Files {
        read: PathBuf,
        write: PathBuf,
        folder: PathBuf,
        copy: PathBuf,
        secret: PathBuf,
        machine: PathBuf,
    }

    fn check() {
        let base = tempfile::Builder::new()
            .prefix("monitor-root")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let at = |name: &str| base.path().join(name);
        let files = Files {
            read: at("read.txt"),
            write: at("write.txt"),
            folder: at("folder"),
            copy: at("copy-of-the-monitor"),
            secret: at("secret.txt"),
            machine: at("machine"),
        };
        fs::write(&files.read, "only to read").unwrap();
        fs::write(&files.write, "before").unwrap();
        fs::write(&files.secret, "nobody's").unwrap();
        fs::create_dir(&files.folder).unwrap();
        fs::create_dir(&files.machine).unwrap();
        let program = std::env::current_exe().unwrap();
        if fs::hard_link(&program, &files.copy).is_err() {
            fs::copy(&program, &files.copy).unwrap();
        }

        let mut sandbox = Sandbox::new(Network::None, vec![], files.machine.join(ROOT));
        sandbox.show(&program, Access::Run);
        // This program's loader and libraries, which cloud-hypervisor, linked statically, does without.
        for libraries in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
            if Path::new(libraries).exists() {
                sandbox.show(libraries, Access::Run);
            }
        }
        sandbox.show(&files.read, Access::Read);
        sandbox.show(&files.write, Access::Write);
        sandbox.show(&files.folder, Access::Write);
        sandbox.show(&files.copy, Access::Read);
        sandbox.show(&files.machine, Access::Write);
        sandbox.show("/dev/null", Access::Write);
        sandbox.show("/dev/urandom", Access::Read);
        let mut monitor: Vec<OsString> = vec![program.clone().into(), PROBES.into()];
        for path in [
            &files.read,
            &files.write,
            &files.folder,
            &files.copy,
            &files.secret,
            &files.machine,
        ] {
            monitor.push(path.clone().into());
        }
        let command = sandbox.command(&program, monitor);
        let done = Command::new(&command[0])
            .args(&command[1..])
            .env(MARK, "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&done.stdout);
        let found: BTreeMap<&str, &str> = said.lines().filter_map(|line| line.split_once('=')).collect();

        let wanted: &[(&str, &str, &str)] = &[
            ("the_monitor_sees_no_process_but_itself", "processes", "1"),
            ("the_monitor_signals_nothing_outside_itself", "signal-all", "3"),
            (
                "the_monitor_has_no_capability",
                "capabilities",
                "0000000000000000",
            ),
            ("the_monitors_proc_names_its_own_root", "proc-root", "/"),
            ("the_hosts_files_are_not_there", "etc-passwd", "2"),
            ("a_file_beside_a_shown_one_is_not_there", "secret", "2"),
            ("up_from_the_root_is_the_root", "parent", "same"),
            ("a_descriptor_the_sandbox_was_handed_is_closed", "leaked", "9"),
            ("a_file_to_read_is_read", "read", "only to read"),
            ("a_file_to_read_is_not_written", "read-write", "30"),
            ("a_file_to_write_is_written", "write", "ok"),
            ("a_folder_to_write_takes_a_new_file", "folder", "ok"),
            ("the_monitors_folder_takes_a_socket", "socket", "ok"),
            ("the_root_takes_no_new_file", "root-write", "30"),
            ("a_file_to_read_is_not_run", "run-copy", "13"),
            ("the_monitor_mounts_nothing", "mount", "1"),
            ("a_device_to_write_is_written", "dev-null", "ok"),
            ("a_device_to_read_is_read", "dev-urandom", "ok"),
            ("the_monitor_ends_as_it_exits", "end", "ok"),
        ];
        let mut failed = Vec::new();
        for (name, key, value) in wanted {
            let got = found.get(key).copied().unwrap_or("nothing");
            if got == *value {
                println!("test {name} ... ok");
            } else {
                println!("test {name} ... FAILED: {key}={got}, wanted {value}");
                failed.push(*name);
            }
        }
        // What the monitor wrote is on the host; what it was to read is as it was.
        for (name, path, wanted) in [
            ("what_the_monitor_wrote_is_on_the_host", &files.write, "written"),
            (
                "what_the_monitor_made_in_a_folder_is_on_the_host",
                &files.folder.join("made"),
                "made",
            ),
            ("the_file_to_read_is_unchanged", &files.read, "only to read"),
            ("the_file_beside_is_unchanged", &files.secret, "nobody's"),
        ] {
            let got = fs::read_to_string(path).unwrap_or_else(|e| format!("({e})"));
            if got == wanted {
                println!("test {name} ... ok");
            } else {
                println!("test {name} ... FAILED: {got:?}, wanted {wanted:?}");
                failed.push(name);
            }
        }
        if !files.machine.join("socket").exists() {
            println!("test the_monitors_socket_is_on_the_host ... FAILED");
            failed.push("the_monitors_socket_is_on_the_host");
        } else {
            println!("test the_monitors_socket_is_on_the_host ... ok");
        }
        let checks = wanted.len() + 5;
        if failed.is_empty() {
            println!("\ntest result: ok. {checks} passed");
        } else {
            panic!(
                "{} failed: {failed:?}\nthe monitor said:\n{said}\nthe sandbox said:\n{}",
                failed.len(),
                String::from_utf8_lossy(&done.stderr)
            );
        }
    }

    /// The error number of a failed call, or `ok`.
    fn code<T>(result: io::Result<T>) -> String {
        match result {
            Ok(_) => "ok".into(),
            Err(error) => error
                .raw_os_error()
                .map_or_else(|| error.to_string(), |n| n.to_string()),
        }
    }

    fn probes(args: &[OsString]) {
        let [read, write, folder, copy, secret, machine] = args else {
            panic!("six paths, not {args:?}");
        };
        let say = |key: &str, value: String| println!("{key}={value}");
        let mut processes: Vec<String> = fs::read_dir("/proc")
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .filter(|name| name.bytes().all(|b| b.is_ascii_digit()))
                    .collect()
            })
            .unwrap_or_default();
        processes.sort();
        say("processes", processes.join(","));
        // SAFETY: kill takes no pointers; signal 0 sends nothing.
        say(
            "signal-all",
            code(if unsafe { libc::kill(-1, 0) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }),
        );
        let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
        say(
            "capabilities",
            status
                .lines()
                .find_map(|line| line.strip_prefix("CapEff:"))
                .unwrap_or("none")
                .trim()
                .into(),
        );
        say(
            "proc-root",
            fs::read_link("/proc/1/root").map_or_else(|e| e.to_string(), |p| p.display().to_string()),
        );
        say("etc-passwd", code(fs::read("/etc/passwd")));
        say("secret", code(fs::read(secret)));
        let names = |path: &str| -> Vec<OsString> {
            let mut names: Vec<OsString> = fs::read_dir(path)
                .map(|e| e.flatten().map(|e| e.file_name()).collect())
                .unwrap_or_default();
            names.sort();
            names
        };
        say(
            "parent",
            if names("/..") == names("/") {
                "same".into()
            } else {
                format!("{:?}", names("/.."))
            },
        );
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat writes the struct, which lives through the call.
        say(
            "leaked",
            code(if unsafe { libc::fstat(LEAKED, &raw mut stat) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }),
        );
        say("read", fs::read_to_string(read).unwrap_or_else(|e| e.to_string()));
        say("read-write", code(fs::OpenOptions::new().write(true).open(read)));
        say("write", code(fs::write(write, "written")));
        say("folder", code(fs::write(Path::new(folder).join("made"), "made")));
        say(
            "socket",
            code(UnixListener::bind(Path::new(machine).join("socket"))),
        );
        say("root-write", code(fs::write("/made", "x")));
        say("run-copy", code(Command::new(copy).arg("--list").output()));
        let target = std::ffi::CString::new(folder.as_encoded_bytes()).unwrap();
        // SAFETY: every pointer is a NUL terminated string that lives through the call.
        let mounted = unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                target.as_ptr(),
                c"tmpfs".as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        say(
            "mount",
            code(if mounted == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }),
        );
        say(
            "dev-null",
            code(fs::OpenOptions::new().write(true).open("/dev/null")),
        );
        let mut random = [0u8; 8];
        say(
            "dev-urandom",
            code(fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut random))),
        );
        say("end", "ok".into());
    }
}
