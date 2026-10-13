//! The trait's contract against a real cloud-hypervisor under KVM: how each end of a machine is told. The guest is
//! the pinned kernel with the supervisor's initramfs (`cargo xtask guest` and `cargo xtask cloud-hypervisor` put
//! them in `target/`), which waits two minutes for a host that never comes; or with an initramfs of nothing, whose
//! kernel finds no init and panics at once; or with an empty kernel, which the monitor refuses. Machines
//! have no network, so pasta is not needed.
//!
//! It needs `/dev/kvm` and a cgroup delegated to the user, as `systemd-run --user --scope -p Delegate=yes cargo test
//! -p cf-vmm-ch --test kvm` gives. Without them it says what is missing and passes, unless `CF_REQUIRE_KVM=1`; CI
//! has no KVM. No test harness: the sandbox this program also is must be the only thread of its process.

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
}

#[cfg(target_os = "linux")]
mod linux {
    use cf_vmm::{Network, Vm, VmExit, VmSpec, Vmm};
    use cf_vmm_ch::cgroup::{self, Delegation};
    use cf_vmm_ch::sandbox::SUBCOMMAND;
    use cf_vmm_ch::{ChVmm, Config};
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    type Check<'a> = dyn Fn() -> Result<(), String> + 'a;
    /// How long any end may take to be told.
    const WAIT: Duration = Duration::from_secs(20);

    pub fn main() {
        let args: Vec<OsString> = std::env::args_os().collect();
        if args.get(1).is_some_and(|arg| arg == SUBCOMMAND) {
            std::process::exit(cf_vmm_ch::sandbox::main(&args[2..]));
        }
        let required = std::env::var_os("CF_REQUIRE_KVM").is_some_and(|value| value == "1");
        let skip = |why: String| {
            if required {
                panic!("CF_REQUIRE_KVM=1, and {why}");
            }
            println!("SKIPPED: {why}");
            std::process::exit(0);
        };
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        let (kernel, initramfs, monitor) = (
            target.join("guest/Image"),
            target.join("guest/initramfs.cpio.gz"),
            target.join("vmm-programs/cloud-hypervisor"),
        );
        for path in [&kernel, &initramfs, &monitor] {
            if !path.exists() {
                skip(format!("{} is not there", path.display()));
            }
        }
        if let Err(error) = fs::OpenOptions::new().read(true).write(true).open("/dev/kvm") {
            skip(format!("/dev/kvm: {error}"));
        }
        if let Err(why) = cgroup::enter_host() {
            skip(why);
        }
        let base = tempfile::Builder::new()
            .prefix("kvm")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let empty = base.path().join("empty.cpio");
        fs::write(&empty, empty_cpio()).unwrap();
        // A kernel of zeros boots and hangs on arm64; an empty one the monitor cannot read and refuses.
        let no_kernel = base.path().join("no-kernel");
        fs::write(&no_kernel, b"").unwrap();
        let vmm = match ChVmm::new(Config {
            dir: base.path().join("machines"),
            cloud_hypervisor: monitor,
            pasta: None,
            sandbox: std::env::current_exe().unwrap(),
            host_cpus: None,
            host_networks: Vec::new(),
        }) {
            Ok(vmm) => vmm,
            Err(error) => return skip(format!("no monitor: {error}")),
        };
        let spec_of = |kernel: &Path, initramfs: &Path, name: &str| VmSpec {
            kernel: kernel.to_path_buf(),
            initramfs: initramfs.to_path_buf(),
            memory_bytes: 512 << 20,
            cpus: 1,
            disks: Vec::new(),
            network: Network::None,
            console: base.path().join(format!("{name}.console")),
        };
        let spec = |initramfs: &Path, name: &str| spec_of(&kernel, initramfs, name);
        let checks: [(&str, &Check); 6] = [
            ("a_kernel_panic_ends_the_machine_as_an_error", &|| {
                // The kernel panics at once, maybe before the machine counts as started.
                let told = match vmm.start(&spec(&empty, "panic")) {
                    Err(error) => error.to_string(),
                    Ok(vm) => format!("{:?}", ended(vm.as_ref())?),
                };
                expect(told.contains("the guest reset its machine"), &told)
            }),
            ("a_stop_ends_the_machine_as_stopped", &|| {
                let vm = vmm.start(&spec(&initramfs, "stop")).map_err(|e| e.to_string())?;
                expect(
                    vm.resident().is_some_and(|bytes| bytes > 0),
                    "nothing read of a running machine",
                )?;
                vm.stop().map_err(|e| e.to_string())?;
                let exit = ended(vm.as_ref())?;
                expect(exit == VmExit::Stopped, &format!("{exit:?}"))
            }),
            ("a_kill_ends_the_machine_as_stopped", &|| {
                let vm = vmm.start(&spec(&initramfs, "kill")).map_err(|e| e.to_string())?;
                vm.kill();
                let exit = ended(vm.as_ref())?;
                expect(exit == VmExit::Stopped, &format!("{exit:?}"))
            }),
            ("a_cgroup_out_of_memory_ends_the_machine_as_an_error", &|| {
                let vm = vmm.start(&spec(&initramfs, "oom")).map_err(|e| e.to_string())?;
                // What the machine holds already is more than this: the kernel kills in its cgroup at once.
                fs::write(machine_group()?.join("memory.max"), "16M").map_err(|e| e.to_string())?;
                let exit = ended(vm.as_ref())?;
                expect(
                    matches!(&exit, VmExit::Error(why) if why.contains("its cgroup allows")),
                    &format!("{exit:?}"),
                )
            }),
            (
                "a_monitor_that_is_killed_ends_the_machine_as_an_error_naming_the_signal",
                &|| {
                    let vm = vmm
                        .start(&spec(&initramfs, "killed"))
                        .map_err(|e| e.to_string())?;
                    let group = machine_group()?;
                    let monitor = fs::read_to_string(group.join("cgroup.procs"))
                        .map_err(|e| e.to_string())?
                        .lines()
                        .filter_map(|pid| pid.parse::<i32>().ok())
                        .find(|pid| {
                            fs::read_to_string(format!("/proc/{pid}/comm"))
                                .is_ok_and(|comm| comm.trim() == "cloud-hyperviso")
                        })
                        .ok_or("no monitor in the machine's cgroup")?;
                    // SAFETY: kill takes no pointers.
                    unsafe { libc::kill(monitor, libc::SIGKILL) };
                    let exit = ended(vm.as_ref())?;
                    expect(
                        matches!(&exit, VmExit::Error(why) if why.starts_with("the monitor was ended by signal 9")),
                        &format!("{exit:?}"),
                    )
                },
            ),
            (
                "a_monitor_that_fails_ends_the_machine_as_an_error_with_what_it_said",
                &|| {
                    let told = match vmm.start(&spec_of(&no_kernel, &initramfs, "refused")) {
                        Err(error) => error.to_string(),
                        Ok(vm) => format!("{:?}", ended(vm.as_ref())?),
                    };
                    expect(
                        told.contains("the monitor exited with code 1")
                            && told.contains("Cannot load the kernel into memory"),
                        &told,
                    )
                },
            ),
        ];
        let mut failed = Vec::new();
        for (name, check) in &checks {
            match check() {
                Ok(()) => println!("test {name} ... ok"),
                Err(why) => {
                    println!("test {name} ... FAILED: {why}");
                    failed.push(*name);
                }
            }
            // Each check's machine is gone before the next starts: one machine cgroup at a time.
            let began = Instant::now();
            while machine_group().is_ok() && began.elapsed() < WAIT {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if !failed.is_empty() {
            panic!("{} failed: {failed:?}", failed.len());
        }
        println!("\ntest result: ok. {} passed", checks.len());
    }

    /// An initramfs with nothing in it: a newc archive of its trailer alone, padded to 512 bytes.
    fn empty_cpio() -> Vec<u8> {
        let name = b"TRAILER!!!\0";
        let mut archive = b"070701".to_vec();
        // ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor, rdevmajor, rdevminor, namesize, check.
        for field in [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, name.len(), 0] {
            archive.extend(format!("{field:08x}").bytes());
        }
        archive.extend(name);
        archive.resize(archive.len().next_multiple_of(4), 0);
        archive.resize(512, 0);
        archive
    }

    fn expect(holds: bool, otherwise: &str) -> Result<(), String> {
        if holds { Ok(()) } else { Err(otherwise.to_string()) }
    }

    fn ended(vm: &dyn Vm) -> Result<VmExit, String> {
        vm.wait(WAIT).ok_or_else(|| format!("not ended within {WAIT:?}"))
    }

    /// The one machine's cgroup there is now.
    fn machine_group() -> Result<PathBuf, String> {
        Delegation::claim()?
            .machines_of(std::process::id())
            .first()
            .map(|group| group.path().to_path_buf())
            .ok_or_else(|| "no machine cgroup".to_string())
    }
}
