//! A vm-host started on Linux as the service starts one: by `systemd-run --user --scope`, in a scope of its own.
//! It must be the process the Runner wrote down, alone in the scope's cgroup, and able to claim that cgroup for its
//! machines; the scope must go once it ends. This test program stands in for the Runner's binary: started as
//! `<it> vm --state-dir <folder>`, it claims the delegation as a real vm-host does and writes down what it found.
//!
//! It needs the user's systemd. Where none runs it says so and passes, unless `CF_REQUIRE_USER_MANAGER` is set.

/// Nothing to try where there is no systemd scope.
#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("vm") {
        linux::stand_in(&args);
    } else {
        linux::a_vm_host_in_a_scope_claims_its_delegated_cgroup_and_the_scope_goes_with_it();
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Read as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use cf_daemon::{VmHosts, Vms};
    use cf_guest_proto::Workload;
    use cf_vmhost::{JobRequest, Layout};

    /// Waits until `done`, for at most `limit`.
    fn within(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let began = Instant::now();
        while !done() {
            if began.elapsed() > limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    /// What the stand-in writes down.
    const FOUND: &str = "found.json";
    /// What the test writes when the stand-in may end.
    const END: &str = "end";

    /// The stand-in vm-host: reads its job, claims the delegation, says what it found and ends.
    pub fn stand_in(args: &[String]) {
        let state = PathBuf::from(&args[3]);
        let mut request = String::new();
        let _ = std::io::stdin().read_to_string(&mut request);
        let own = cf_vmm_ch::cgroup::own();
        let alone = own
            .as_ref()
            .ok()
            .and_then(|own| std::fs::read_to_string(own.join("cgroup.procs")).ok())
            .map(|procs| procs.lines().count());
        let claim = cf_vmm_ch::cgroup::Delegation::claim();
        let found = serde_json::json!({
            "pid": std::process::id(),
            "job": serde_json::from_str::<serde_json::Value>(&request).ok().map(|request| request["job"].clone()),
            "cgroup": own.map(|own| own.display().to_string()).ok(),
            "processes": alone,
            "after": cf_vmm_ch::cgroup::own().map(|own| own.display().to_string()).ok(),
            "machines": claim.as_ref().map(|d| d.machines().display().to_string()).map_err(Clone::clone),
        });
        std::fs::write(state.join(FOUND), found.to_string()).expect("written");
        // Runs on while the test looks at its scope.
        within(Duration::from_secs(30), || state.join(END).exists());
    }

    fn user_manager() -> Result<(), String> {
        let output = Command::new("/usr/bin/systemctl")
            .args(["--user", "show-environment"])
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    fn show(unit: &str, property: &str) -> String {
        let output = Command::new("/usr/bin/systemctl")
            .args([
                "--user",
                "show",
                &format!("--property={property}"),
                "--value",
                unit,
            ])
            .output()
            .expect("systemctl");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    pub fn a_vm_host_in_a_scope_claims_its_delegated_cgroup_and_the_scope_goes_with_it() {
        if let Err(why) = user_manager() {
            assert!(
                std::env::var_os("CF_REQUIRE_USER_MANAGER").is_none(),
                "no systemd user manager: {why}"
            );
            eprintln!("SKIPPED: no systemd user manager: {why}");
            return;
        }
        let folder = tempfile::tempdir().expect("folder");
        let state = folder.path();
        let program = std::env::current_exe().expect("this program");
        let vms = VmHosts::new(&program, state).in_scopes("/usr/bin/systemd-run");
        let job = format!("jscope{}", std::process::id());
        let unit = format!("codefusion-runner-{job}.scope");
        let before = cf_vmm_ch::cgroup::own().expect("the test's own cgroup");
        vms.start(&JobRequest {
            job: job.clone(),
            host: "host-a".into(),
            memory_bytes: 2 << 30,
            cpus: 2,
            scratch_bytes: 1 << 30,
            image: "2026.10.1".into(),
            workload: Workload::Command {
                argv: vec!["true".into()],
            },
            env: Vec::new(),
            deadline_s: 60,
        })
        .expect("started");
        let found_at = state.join(FOUND);
        assert!(
            within(Duration::from_secs(20), || found_at.exists()),
            "the vm-host never wrote what it found"
        );
        std::thread::sleep(Duration::from_millis(100));
        let found: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&found_at).expect("found")).expect("json");
        // The process systemd-run became is the one the Runner wrote down before it handed over the job.
        let noted: serde_json::Value = serde_json::from_slice(
            &std::fs::read(Layout::new(state).run().join(format!("{job}.pid"))).expect("the note"),
        )
        .expect("json");
        assert_eq!(found["pid"], noted["pid"], "{found}");
        assert_eq!(
            found["job"],
            serde_json::json!(job),
            "it got its job on its input"
        );
        let cgroup = PathBuf::from(found["cgroup"].as_str().expect("a cgroup"));
        assert_eq!(
            cgroup.file_name().and_then(|name| name.to_str()),
            Some(unit.as_str()),
            "{found}"
        );
        assert_eq!(found["processes"], 1, "alone in its scope: {found}");
        assert_eq!(
            found["machines"]["Ok"].as_str().map(Path::new),
            Some(cgroup.join("machines").as_path()),
            "the scope's cgroup is delegated to it and it claimed it: {found}"
        );
        assert_eq!(
            found["after"].as_str().map(Path::new),
            Some(cgroup.join("host").as_path()),
            "it moved itself into the scope's `host` leaf: {found}"
        );
        assert_eq!(
            cf_vmm_ch::cgroup::own().expect("the test's own cgroup"),
            before,
            "the Runner stays where it was"
        );
        // systemd knows the cgroup is the vm-host's to manage, and leaves what it made there alone.
        assert_eq!(show(&unit, "Delegate"), "yes");
        assert_eq!(show(&unit, "ActiveState"), "active");
        std::fs::write(state.join(END), "").expect("the end");
        // The vm-host is gone, and with it the scope and its cgroup.
        assert!(
            within(Duration::from_secs(20), || !cgroup.exists()
                && matches!(show(&unit, "ActiveState").as_str(), "inactive" | "")),
            "the scope stays: {}",
            show(&unit, "ActiveState")
        );
        eprintln!("ok: a vm-host in {unit} claimed {}", cgroup.display());
    }
}
