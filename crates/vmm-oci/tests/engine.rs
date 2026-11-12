//! Whole jobs in a real engine's containers, through the vm-host as the Runner runs them: what a job can and cannot
//! reach, and how it ends. Ignored by default; a host with an engine runs them with
//!
//! ```text
//! CF_ENGINE_SOCKET=/run/user/1001/docker.sock   the engine's API socket
//! CF_SUPERVISOR=/path/to/supervisor             the guest's supervisor, a static Linux binary
//! CF_GUEST_IMAGE=<version>                      a version whose codefusion-runner-guest:<version> the engine holds
//! CF_ENGINE_ROOTFUL=1                           only on a throwaway host whose engine is not rootless
//! cargo test -p cf-vmm-oci --test engine -- --ignored --test-threads=2
//! ```
//!
//! The image needs no more than a shell, `sleep`, `head`, `tail` and a user `runner`, so a `busybox` image with
//! that user stands in for the guest image.

use cf_guest_proto::{Secret, Workload};
use cf_image::store::{Store, Target};
use cf_image::{ReleaseInputs, RootfsKind, write_release};
use cf_vmhost::{End, Guest, JobRequest, JobState, Layout, Limits, run};
use cf_vmm_oci::http::{Body, Engine};
use cf_vmm_oci::image::Images;
use cf_vmm_oci::{Config, OciVmm, image_tag};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const IGNORED: &str = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE";

fn setting(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set: {IGNORED}"))
}

fn socket() -> PathBuf {
    setting("CF_ENGINE_SOCKET").into()
}

/// A machine name of this test and process alone, so that two runs on one engine never share containers.
fn machine(test: &str) -> String {
    format!("it-{test}-{}", std::process::id())
}

fn engine(machine: &str) -> OciVmm {
    OciVmm::new(Config {
        socket: socket(),
        machine: machine.into(),
        rootful_for_tests: std::env::var("CF_ENGINE_ROOTFUL").is_ok_and(|value| value == "1"),
    })
    .expect("the engine answers")
}

/// The supervisor as `init` of a gzip-compressed `newc` archive, as the guest's initramfs holds it.
fn initramfs() -> Vec<u8> {
    let supervisor = std::fs::read(setting("CF_SUPERVISOR")).expect("the supervisor");
    let mut cpio = Vec::new();
    let mut add = |name: &str, data: &[u8]| {
        let fields = [
            1,
            0o100_755,
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            0,
            0,
            name.len() + 1,
            0,
        ];
        cpio.extend_from_slice(b"070701");
        for value in fields {
            cpio.extend_from_slice(format!("{value:08X}").as_bytes());
        }
        cpio.extend_from_slice(name.as_bytes());
        cpio.push(0);
        cpio.resize(cpio.len().next_multiple_of(4), 0);
        cpio.extend_from_slice(data);
        cpio.resize(cpio.len().next_multiple_of(4), 0);
    };
    add("init", &supervisor);
    add("TRAILER!!!", b"");
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    gz.write_all(&cpio).expect("compressed");
    gz.finish().expect("compressed")
}

fn request(job: &str, script: &str, deadline_s: u64) -> JobRequest {
    JobRequest {
        job: job.into(),
        host: "it".into(),
        memory_bytes: 512 << 20,
        cpus: 1,
        scratch_bytes: 1 << 30,
        image: setting("CF_GUEST_IMAGE"),
        workload: Workload::Command {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        },
        env: vec![("CI".into(), Secret::new("true".into()))],
        deadline_s,
        docker: false,
        trusted: None,
    }
}

/// Waits short enough for a test: a job told to end at its deadline has two seconds, then its container goes.
fn limits(deadline: Duration) -> Limits {
    Limits {
        deadline_grace: Duration::from_secs(2),
        stop_slack: Duration::from_secs(2),
        kill: Duration::from_secs(5),
        ..Limits::for_deadline(deadline)
    }
}

/// One job in a container of `vmm`: its last state and what it wrote.
fn job(
    vmm: &OciVmm,
    folder: &tempfile::TempDir,
    request: JobRequest,
    cancel: &AtomicBool,
) -> (JobState, String) {
    let layout = Layout::new(folder.path());
    let name = request.job.clone();
    let deadline = Duration::from_secs(request.deadline_s);
    let initramfs = initramfs();
    let guest = Guest {
        kernel: b"",
        initramfs: &initramfs,
    };
    let state = run(vmm, &layout, guest, request, limits(deadline), cancel).expect("the job runs");
    let console = std::fs::read_to_string(layout.console(&name)).unwrap_or_default();
    // Shown when the test fails: how the job ended, and what its container wrote.
    eprintln!("{name}: {state:?}\n{console}");
    (state, console)
}

/// Nothing of the machine's is left in the engine: no container, no network.
fn nothing_left(vmm: &OciVmm) {
    assert_eq!(vmm.containers(), [], "the container is gone with the job");
    let left = vmm.networks();
    for (network, _) in &left {
        // Shown when the test fails: what the engine says of the network, and to its removal now. The test fails on
        // what was left, whatever this removal does.
        let engine = Engine::new(socket());
        let call = Some(Duration::from_secs(60));
        let found = engine.request("GET", &format!("/networks/{network}"), Body::Empty, call);
        let removed = engine.request("DELETE", &format!("/networks/{network}"), Body::Empty, call);
        eprintln!(
            "left: {network}: {:?}; removed now: {:?}",
            found.map(|answer| (answer.status, String::from_utf8_lossy(&answer.body).into_owned())),
            removed.map(|answer| (answer.status, answer.message()))
        );
    }
    assert_eq!(left, [], "the network is gone with the job");
}

fn end(state: &JobState) -> Option<End> {
    state.end.as_ref().map(|ended| ended.end)
}

fn exit_code(state: &JobState) -> Option<i32> {
    state.end.as_ref()?.summary.as_ref()?.exit_code
}

#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn a_job_cannot_read_a_host_file_outside_its_container() {
    let host = tempfile::tempdir().expect("a folder");
    let secret = host.path().join("cf-engine-secret");
    std::fs::write(&secret, "the-host-secret-4e1f").expect("written");
    let name = secret.file_name().unwrap().to_str().unwrap();
    // It may look anywhere it can: the file is not there, under its path or any other.
    let script = format!(
        "cat {path}; found=$(find / -xdev -name {name} 2>/dev/null; find /proc/1/root -maxdepth 3 -name {name} \
         2>/dev/null); echo \"found:[$found]\"; test -z \"$found\" && ! test -e {path}",
        path = secret.display()
    );
    let vmm = engine(&machine("host-file"));
    let folder = tempfile::tempdir().expect("a state folder");
    let (state, console) = job(
        &vmm,
        &folder,
        request("host-file", &script, 60),
        &AtomicBool::new(false),
    );
    assert_eq!(
        (end(&state), exit_code(&state)),
        (Some(End::Ok), Some(0)),
        "{console}"
    );
    assert!(console.contains("found:[]"), "{console}");
    assert!(!console.contains("the-host-secret-4e1f"), "{console}");
    nothing_left(&vmm);
}

#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn a_job_cannot_reach_the_engines_socket() {
    let socket = socket();
    let script = format!(
        "sockets=$(find / -xdev -type s 2>/dev/null); echo \"sockets:[$sockets]\"; \
         for path in {} /var/run/docker.sock /run/docker.sock /run/podman/podman.sock; do \
           test -e \"$path\" && echo \"there: $path\" && exit 1; done; \
         echo uid=$(id -u); test \"$(id -u)\" != 0",
        socket.display()
    );
    let vmm = engine(&machine("socket"));
    let folder = tempfile::tempdir().expect("a state folder");
    let (state, console) = job(
        &vmm,
        &folder,
        request("socket", &script, 60),
        &AtomicBool::new(false),
    );
    assert_eq!(
        (end(&state), exit_code(&state)),
        (Some(End::Ok), Some(0)),
        "{console}"
    );
    assert!(
        !console.contains("docker.sock") && !console.contains("podman.sock"),
        "{console}"
    );
    nothing_left(&vmm);
}

#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn a_job_past_its_memory_is_stopped_for_memory() {
    // A string that doubles until it holds more than the container's 512 MB.
    let vmm = engine(&machine("memory"));
    let folder = tempfile::tempdir().expect("a state folder");
    let began = Instant::now();
    let (state, console) = job(
        &vmm,
        &folder,
        request(
            "memory",
            "awk 'BEGIN { s = \"xxxxxxxx\"; for (;;) s = s s }'; echo \"awk: $?\"",
            120,
        ),
        &AtomicBool::new(false),
    );
    assert_eq!(end(&state), Some(End::Oom), "{console}");
    // The kernel killed the process past the cap, not the container's first process: the job's shell heard of it.
    assert!(console.contains("awk: 137"), "{console}");
    assert!(began.elapsed() < Duration::from_secs(90), "{:?}", began.elapsed());
    nothing_left(&vmm);
}

#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn a_job_past_its_deadline_is_stopped_and_its_container_removed() {
    let vmm = engine(&machine("deadline"));
    let folder = tempfile::tempdir().expect("a state folder");
    let began = Instant::now();
    // It ignores the polite stop, so only the kill at the end of the grace ends it.
    let (state, console) = job(
        &vmm,
        &folder,
        request("deadline", "trap '' TERM INT; while :; do sleep 1; done", 3),
        &AtomicBool::new(false),
    );
    assert_eq!(end(&state), Some(End::Timeout), "{console}");
    let took = began.elapsed();
    assert!(
        took >= Duration::from_secs(3) && took < Duration::from_secs(30),
        "{took:?}"
    );
    nothing_left(&vmm);
}

/// What a vm-host leaves when it dies mid-job, killed with no word, is gone when the next one starts.
#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn what_a_vm_host_killed_mid_job_leaves_is_removed_when_the_next_starts() {
    // The child is told the parent's name for the machine: its own process number differs.
    let name = std::env::var("CF_ENGINE_MACHINE").unwrap_or_else(|_| machine("crash"));
    if let Ok(folder) = std::env::var("CF_ENGINE_CHILD") {
        // The vm-host that will be killed: a job that never ends on its own.
        let vmm = engine(&name);
        let folder = tempfile::TempDir::new_in(folder).expect("a state folder");
        job(
            &vmm,
            &folder,
            request("crash", "sleep 600", 600),
            &AtomicBool::new(false),
        );
        return;
    }
    let parent = tempfile::tempdir().expect("a folder");
    let mut child = std::process::Command::new(std::env::current_exe().expect("this test"))
        .args([
            "--ignored",
            "--exact",
            "what_a_vm_host_killed_mid_job_leaves_is_removed_when_the_next_starts",
            "--nocapture",
        ])
        .env("CF_ENGINE_CHILD", parent.path())
        .env("CF_ENGINE_MACHINE", &name)
        .spawn()
        .expect("the child starts");
    let watcher = engine(&name);
    let began = Instant::now();
    while watcher.containers().is_empty() && began.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let made = watcher.containers();
    assert_eq!(made.len(), 1, "the child's job runs in a container");
    assert_eq!(watcher.networks().len(), 1, "on a network of its own");
    // The job is under way in it: killed now, its vm-host leaves it mid-job.
    std::thread::sleep(Duration::from_secs(2));
    // SAFETY: kill takes a number and a signal; the number is our own child's, whose exit nobody collected yet.
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
    assert_eq!(
        watcher.containers(),
        made,
        "nothing removes it while no vm-host starts"
    );
    let next = engine(&name);
    nothing_left(&next);
}

/// Two jobs at once on one host: neither sees the other's files or processes.
#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn two_jobs_at_once_do_not_see_each_others_files() {
    let name = machine("two");
    let vmm = std::sync::Arc::new(engine(&name));
    let cancel_first = std::sync::Arc::new(AtomicBool::new(false));
    let first = {
        let (vmm, cancel) = (vmm.clone(), cancel_first.clone());
        std::thread::spawn(move || {
            let folder = tempfile::tempdir().expect("a state folder");
            job(
                &vmm,
                &folder,
                request(
                    "first",
                    "echo first-secret-91c2 > /tmp/first-secret; echo first-secret-91c2 > ~/first-secret; \
                     exec sleep 600",
                    600,
                ),
                &cancel,
            )
        })
    };
    let began = Instant::now();
    while vmm.containers().is_empty() && began.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_secs(2));
    let folder = tempfile::tempdir().expect("a state folder");
    let (second, console) = job(
        &vmm,
        &folder,
        request(
            "second",
            "ls /tmp ~; found=$(grep -rl first-secret-91c2 /tmp /home /run 2>/dev/null; ps -o args 2>/dev/null | \
             grep -c '[s]leep 600'); echo \"found:[$found]\"; test \"$found\" = 0",
            60,
        ),
        &AtomicBool::new(false),
    );
    cancel_first.store(true, Ordering::Relaxed);
    let (first, _) = first.join().expect("the first job ends");
    assert_eq!(
        (end(&second), exit_code(&second)),
        (Some(End::Ok), Some(0)),
        "{console}"
    );
    assert!(!console.contains("first-secret"), "{console}");
    assert_eq!(end(&first), Some(End::Cancelled));
    nothing_left(&vmm);
}

/// The files of the test image, as the image workflow exports the job image's: a container made from it, exported.
fn exported_image() -> Vec<u8> {
    let engine = Engine::new(socket());
    let call = Some(Duration::from_secs(60));
    let body = serde_json::json!({ "Image": image_tag(&setting("CF_GUEST_IMAGE")) });
    let made = engine
        .request("POST", "/containers/create", Body::Json(&body), call)
        .expect("the engine answers");
    assert!(made.ok(), "{}", made.message());
    let id = made.json().unwrap()["Id"].as_str().unwrap().to_string();
    let export = engine.request("GET", &format!("/containers/{id}/export"), Body::Empty, call);
    let _ = engine.request("DELETE", &format!("/containers/{id}?force=1"), Body::Empty, call);
    let export = export.expect("exported");
    assert!(export.ok(), "{}", export.message());
    export.body
}

/// A container-image release installed into the engine runs jobs: the store verifies it, the engine imports and
/// names it, and a container made from it, which has none of the first image's settings, runs a job as `runner`.
#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn a_container_image_release_installed_into_the_engine_runs_a_job() {
    let source = tempfile::tempdir().expect("a folder");
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    gz.write_all(&exported_image()).expect("compressed");
    let tar = source.path().join("rootfs.tar.gz");
    std::fs::write(&tar, gz.finish().expect("compressed")).expect("written");
    std::fs::write(source.path().join("vmlinux"), b"unused here").expect("written");
    std::fs::write(source.path().join("initramfs.cpio.gz"), initramfs()).expect("written");
    let version = format!("it-imported-{}", std::process::id());
    let release = tempfile::tempdir().expect("a folder");
    let signer = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    write_release(
        release.path(),
        &ReleaseInputs {
            version: &version,
            built: 1,
            arch: cf_image::HOST_ARCH,
            kernel: &source.path().join("vmlinux"),
            initramfs: &source.path().join("initramfs.cpio.gz"),
            rootfs: Some(&tar),
            rootfs_kind: RootfsKind::TarGz,
            // Several parts, read back as one.
            max_part: 1 << 20,
        },
        &signer,
    )
    .expect("a release");
    let state = tempfile::tempdir().expect("a state folder");
    let mut images = Images::new(&socket());
    Store::new(state.path().join("images"))
        .install_to(
            release.path(),
            &version,
            &signer.verifying_key(),
            Target::Engine(&mut images),
        )
        .expect("installed");

    let vmm = engine(&machine("imported"));
    let folder = tempfile::tempdir().expect("a state folder");
    let mut request = request("imported", "echo uid=$(id -u); test \"$(id -u)\" = 1001", 60);
    request.image = version.clone();
    let (state, console) = job(&vmm, &folder, request, &AtomicBool::new(false));
    let held = Engine::new(socket())
        .request(
            "GET",
            &format!("/images/{}/json", image_tag(&version)),
            Body::Empty,
            Some(Duration::from_secs(60)),
        )
        .expect("the engine answers");
    if let Some(id) = held
        .json()
        .ok()
        .and_then(|image| Some(image.get("Id")?.as_str()?.to_string()))
    {
        cf_image::store::Engine::remove(&mut images, &id);
    }
    assert!(
        held.ok(),
        "the engine holds {}: {}",
        image_tag(&version),
        held.message()
    );
    assert_eq!(
        (end(&state), exit_code(&state)),
        (Some(End::Ok), Some(0)),
        "{console}"
    );
    assert!(console.contains("uid=1001"), "{console}");
    nothing_left(&vmm);
}

/// A machine's firewall (`cf-vmm-ch` `firewall.rs`) drops `cf_nft::BLOCKED`, 127/8 and
/// every network of the host; a container has nothing like it. A listener on all the host's addresses must not hear
/// the job, whatever name or address the job tries, and nor must the cloud's metadata service.
#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn a_job_reaches_no_listener_of_the_host_and_no_metadata() {
    let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("a listener");
    listener.set_nonblocking(true).expect("non-blocking");
    let port = listener.local_addr().expect("its port").port();
    // The host's own addresses, as the job could learn them from DNS or the internet.
    let own = std::process::Command::new("hostname")
        .arg("-I")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    let script = format!(
        "gw=$(ip route 2>/dev/null | awk '/default/ {{print $3}}'); \
         for t in host.containers.internal host.docker.internal host.orb.internal 169.254.1.2 10.0.2.2 \
                  127.0.0.1 $gw {own}; do \
           wget -q -T 3 -O /dev/null http://$t:{port}/ 2>/dev/null; echo tried:$t; done; \
         out=$(wget -T 3 -O /dev/null http://169.254.169.254/ 2>&1); echo \"metadata:[$out]\"; true"
    );
    let vmm = engine(&machine("egress"));
    let folder = tempfile::tempdir().expect("a state folder");
    let (state, console) = job(
        &vmm,
        &folder,
        request("egress", &script, 120),
        &AtomicBool::new(false),
    );
    let heard = std::iter::from_fn(|| listener.accept().ok())
        .map(|(_, from)| from)
        .collect::<Vec<_>>();
    assert_eq!(end(&state), Some(End::Ok), "{console}");
    assert_eq!(heard, [], "the job reached a listener of the host: {console}");
    assert!(
        console.contains("metadata:[") && !console.contains("200 OK") && !console.contains("saving to"),
        "the job reached 169.254.169.254: {console}"
    );
    nothing_left(&vmm);
}

/// A job gains no capabilities in a user namespace of its own: in a machine that is the guest kernel's attack
/// surface, in a container the host's (nf_tables and the rest).
#[test]
#[ignore = "needs a container engine: CF_ENGINE_SOCKET, CF_SUPERVISOR and CF_GUEST_IMAGE"]
fn a_job_makes_no_user_namespace() {
    let vmm = engine(&machine("userns"));
    let folder = tempfile::tempdir().expect("a state folder");
    let (state, console) = job(
        &vmm,
        &folder,
        request(
            "userns",
            "if unshare -Urn sh -c 'grep CapEff /proc/self/status'; then echo made-userns; fi; true",
            60,
        ),
        &AtomicBool::new(false),
    );
    assert_eq!(end(&state), Some(End::Ok), "{console}");
    assert!(!console.contains("made-userns"), "{console}");
    nothing_left(&vmm);
}
