//! A whole job against a scripted machine: what the vm-host tells the guest, what it leaves on disk, and how it
//! ends the ways a job goes wrong.

use cf_guest_proto::{
    GuestEnd, GuestMessage, HostMessage, PROTOCOL, Secret, StopReason, Summary, Task, Usage, Workload,
    read_message, write_message,
};
use cf_vmhost::control::{self, Request};
use cf_vmhost::{End, Guest, JobRequest, JobState, Layout, Limits, RunError, Stage, VM_OVERHEAD, run};
use cf_vmm::testing::{FakeVmm, PowerSwitch, Script, Stop};
use cf_vmm::{Network, VmmError};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TOKEN: &str = "jit-registration-do-not-leak-7f3a";
const GUEST: Guest<'static> = Guest {
    kernel: b"kernel",
    initramfs: b"initramfs",
};

fn request(job: &str) -> JobRequest {
    JobRequest {
        job: job.into(),
        host: "mac-1".into(),
        memory_bytes: 4 << 30,
        cpus: 4,
        scratch_bytes: 1 << 30,
        image: "image-1".into(),
        workload: Workload::ActionsRunner {
            jit_config: Secret::new(TOKEN.to_string()),
        },
        env: vec![("CI".into(), "true".into())],
        deadline_s: 3600,
        docker: false,
        docker_cache: None,
    }
}

/// A state folder with the image installed.
fn state_dir() -> (tempfile::TempDir, Layout) {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::new(dir.path());
    std::fs::create_dir_all(layout.rootfs("image-1").parent().unwrap()).unwrap();
    std::fs::write(layout.rootfs("image-1"), b"rootfs").unwrap();
    (dir, layout)
}

/// Waits that make a test's seconds milliseconds.
fn quick() -> Limits {
    let ms = Duration::from_millis;
    Limits {
        ready: ms(400),
        heartbeat: ms(400),
        deadline: ms(60_000),
        deadline_grace: ms(100),
        stop_slack: ms(200),
        done: ms(300),
        kill: ms(200),
        boots: 2,
        publish: ms(0),
        console_bytes: 1 << 20,
    }
}

fn summary(end: GuestEnd, exit_code: Option<i32>) -> Summary {
    Summary {
        end,
        exit_code,
        signal: None,
        usage: usage(),
        disk_peak_bytes: 3,
        oom: None,
        wall_ms: 4,
        docker_cache: None,
    }
}

fn usage() -> Usage {
    Usage {
        memory_bytes: 100,
        memory_peak_bytes: 200,
        cpu_usec: 300,
        disk_bytes: 400,
        pids: 5,
    }
}

fn say(stream: &mut UnixStream, message: GuestMessage) {
    let _ = write_message(stream, &message);
}

/// The guest's opening: hello, then the task the host gives.
fn greet(stream: &mut UnixStream) -> Option<Task> {
    say(
        stream,
        GuestMessage::Hello {
            protocol: PROTOCOL,
            kernel: "6.18".into(),
        },
    );
    match read_message::<HostMessage>(stream) {
        Ok(Some(HostMessage::Start(start))) => Some(start.task),
        _ => None,
    }
}

/// The machine that formats the first job's disk.
fn formatter() -> Script {
    Script::boot(|mut stream, _| {
        assert!(matches!(greet(&mut stream), Some(Task::FormatScratch)));
        say(&mut stream, GuestMessage::Ready { boot_ms: 1 });
        say(
            &mut stream,
            GuestMessage::Exited(summary(GuestEnd::Exited, Some(0))),
        );
        let _ = read_message::<HostMessage>(&mut stream);
    })
}

/// A machine that starts the job, says it is ready and then does `rest`.
fn job(rest: impl FnOnce(UnixStream, PowerSwitch, Task) + Send + 'static) -> Script {
    Script::boot(move |mut stream, power| {
        let Some(task) = greet(&mut stream) else { return };
        say(&mut stream, GuestMessage::Ready { boot_ms: 1 });
        rest(stream, power, task);
    })
}

/// Ends as a guest does: the summary, then power off once the host has closed the channel.
fn finish(mut stream: UnixStream, summary: Summary) {
    say(&mut stream, GuestMessage::Exited(summary));
    while let Ok(Some(_)) = read_message::<HostMessage>(&mut stream) {}
}

fn go(vmm: &FakeVmm, layout: &Layout, request: JobRequest, limits: Limits) -> Result<JobState, RunError> {
    run(vmm, layout, GUEST, request, limits, &AtomicBool::new(false))
}

/// Runs a job on a thread of its own and fails the test when it has no end within ten seconds: the waits these
/// tests are about are there so that a job always has one.
fn go_within(
    vmm: &Arc<FakeVmm>,
    layout: &Layout,
    request: JobRequest,
    limits: Limits,
    cancel: bool,
) -> Result<JobState, RunError> {
    let (done, result) = mpsc::channel();
    let (vmm, layout) = (vmm.clone(), layout.clone());
    std::thread::spawn(move || {
        let cancel = AtomicBool::new(cancel);
        let _ = done.send(run(vmm.as_ref(), &layout, GUEST, request, limits, &cancel));
    });
    result
        .recv_timeout(Duration::from_secs(10))
        .expect("the job had no end within ten seconds")
}

fn mode(path: impl AsRef<Path>) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn every_file(dir: &Path, found: &mut Vec<(String, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        if entry.path().is_dir() {
            every_file(&entry.path(), found);
        } else if entry.metadata().unwrap().len() < 16 << 20 {
            // All but the empty disks, which are a gigabyte of holes.
            found.push((
                entry.path().display().to_string(),
                std::fs::read(entry.path()).unwrap_or_default(),
            ));
        }
    }
}

#[test]
fn a_job_gets_its_machine_its_limits_and_its_registration_and_leaves_only_its_state() {
    let (dir, layout) = state_dir();
    let (seen, tasks) = mpsc::channel();
    let vmm = FakeVmm::new([
        formatter(),
        job(move |mut stream, _, task| {
            seen.send(task).unwrap();
            say(&mut stream, GuestMessage::Heartbeat(usage()));
            finish(stream, summary(GuestEnd::Exited, Some(0)));
        }),
    ]);
    let state = go(&vmm, &layout, request("job-1"), quick()).unwrap();

    assert_eq!(
        (state.stage, state.end.as_ref().map(|ended| ended.end)),
        (Stage::Ended, Some(End::Ok))
    );
    assert_eq!(
        (state.vm_bytes, state.limit_bytes, state.cpus, state.boots),
        (4 << 30, (4 << 30) - VM_OVERHEAD, 4, 1)
    );
    assert_eq!(
        state.end.as_ref().unwrap().summary.as_ref().unwrap().usage,
        usage()
    );
    assert!(state.boot_ms.is_some());
    assert_eq!(
        serde_json::from_slice::<JobState>(&std::fs::read(layout.state("job-1")).unwrap()).unwrap(),
        state
    );

    let Task::Job(task) = tasks.recv().unwrap() else {
        panic!("the guest was not given a job")
    };
    assert_eq!(
        (task.name.as_str(), task.host.as_str(), task.oom_grace_ms),
        ("job-1", "mac-1", 15_000)
    );
    assert_eq!(
        (
            task.limits.memory_bytes,
            task.limits.pids,
            task.limits.shm_bytes,
            task.limits.cpus
        ),
        ((4 << 30) - (256 << 20), 4096, 1 << 30, 4)
    );
    assert_eq!(task.env, [("CI".to_string(), "true".to_string())]);
    // The guest is told the networks this machine is in, to keep the job from them.
    assert_eq!(task.blocked, cf_vmhost::host_networks());
    assert!(matches!(&task.workload, Workload::ActionsRunner { jit_config } if jit_config.expose() == TOKEN));

    let specs = vmm.started();
    assert_eq!(specs.len(), 2);
    // The formatting machine is small, has no network and the job's future disk; the job's has the size asked.
    assert_eq!(
        (specs[0].memory_bytes, specs[0].network),
        (1 << 30, Network::None)
    );
    let job_spec = &specs[1];
    assert_eq!(
        (job_spec.memory_bytes, job_spec.cpus, job_spec.network),
        (4 << 30, 4, Network::Nat)
    );
    let disks: Vec<_> = job_spec
        .disks
        .iter()
        .map(|disk| (disk.id.as_str(), disk.read_only))
        .collect();
    assert_eq!(disks, [("cf-image", true), ("cf-scratch", false)]);
    assert_eq!(job_spec.disks[0].path, layout.rootfs("image-1"));
    assert!(job_spec.disks[1].path.starts_with(layout.job_dir("job-1")));
    assert_eq!(job_spec.console, layout.console("job-1"));

    // The machine's files and the socket are gone; the registration is in no file and no name.
    assert!(!layout.job_dir("job-1").exists() && !layout.socket("job-1").exists());
    let mut files = Vec::new();
    every_file(dir.path(), &mut files);
    for (name, bytes) in &files {
        assert!(
            !name.contains(TOKEN)
                && !bytes
                    .windows(TOKEN.len())
                    .any(|window| window == TOKEN.as_bytes()),
            "{name} holds the registration"
        );
    }
    assert!(
        !format!("{:?}", request("x")).contains(TOKEN),
        "a request printed for a log shows the registration"
    );
    // What stays is the job's state, its console log (empty here: the scripted machine writes none) and the empty
    // disk the next job of this image and size starts from.
    assert_eq!(std::fs::read(layout.console("job-1")).unwrap(), b"");
    let template = layout.scratch_templates().join("image-1-1073741824.img");
    assert_eq!(std::fs::metadata(template).unwrap().len(), 1 << 30);
    let mut kept: Vec<_> = files
        .iter()
        .map(|(name, _)| name.rsplit('/').next().unwrap())
        .collect();
    kept.sort_unstable();
    assert_eq!(
        kept,
        [
            "image-1-1073741824.lock",
            "job-1.console.log",
            "job-1.json",
            "rootfs.img"
        ]
    );
}

#[test]
fn the_second_job_of_an_image_and_size_formats_nothing() {
    let (_dir, layout) = state_dir();
    let quiet = || job(|stream, _, _| finish(stream, summary(GuestEnd::Exited, Some(0))));
    let vmm = FakeVmm::new([formatter(), quiet(), quiet()]);
    go(&vmm, &layout, request("a"), quick()).unwrap();
    go(&vmm, &layout, request("b"), quick()).unwrap();
    assert_eq!(vmm.started().len(), 3);
    // Each job wrote to its own copy.
    assert_ne!(vmm.started()[1].disks[1].path, vmm.started()[2].disks[1].path);
}

#[test]
fn a_disk_that_cannot_be_formatted_starts_no_job_and_leaves_no_template() {
    let (_dir, layout) = state_dir();
    let vmm = FakeVmm::new([Script::boot(|mut stream, _| {
        greet(&mut stream);
        say(
            &mut stream,
            GuestMessage::Failed {
                stage: "format".into(),
                message: "no mke2fs".into(),
            },
        );
    })]);
    let error = go(&vmm, &layout, request("a"), quick()).unwrap_err();
    assert!(
        matches!(&error, RunError::Scratch(why) if why.contains("no mke2fs")),
        "{error}"
    );
    assert_eq!(vmm.started().len(), 1);
    assert_eq!(
        std::fs::read_dir(layout.scratch_templates())
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e != "lock"))
            .count(),
        0
    );
    assert!(!layout.job_dir("a").exists() && !layout.state("a").exists());
}

#[test]
fn a_guest_that_never_answers_is_booted_twice_and_the_job_fails_to_boot() {
    let (_dir, layout) = state_dir();
    let mute = || Script::Boot {
        refuse_channel: usize::MAX,
        guest: Box::new(|_, _| {}),
        stop: Stop::Obeys,
    };
    let vmm = FakeVmm::new([formatter(), mute(), mute()]);
    let state = go(&vmm, &layout, request("a"), quick()).unwrap();
    let ended = state.end.unwrap();
    assert_eq!((ended.end, state.boots), (End::BootFailed, 2));
    assert!(ended.message.unwrap().contains("not ready"));
    assert_eq!(vmm.started().len(), 3);
    assert!(vmm.stops() >= 2, "both machines were stopped");
}

#[test]
fn a_machine_that_cannot_start_is_tried_twice() {
    let (_dir, layout) = state_dir();
    let refuse = || Script::FailToStart(VmmError::Start("no memory for it".into()));
    let vmm = FakeVmm::new([formatter(), refuse(), refuse()]);
    let ended = go(&vmm, &layout, request("a"), quick()).unwrap().end.unwrap();
    assert_eq!(ended.end, End::BootFailed);
    assert!(ended.message.unwrap().contains("no memory for it"));
}

#[test]
fn a_guest_that_goes_silent_is_lost_and_its_machine_stopped() {
    let (_dir, layout) = state_dir();
    let vmm = FakeVmm::new([formatter(), job(|_stream, power, _| power.wait_for_the_end())]);
    let state = go(&vmm, &layout, request("a"), quick()).unwrap();
    assert_eq!(state.end.unwrap().end, End::GuestLost);
    assert!(vmm.stops() >= 1);
}

#[test]
fn a_machine_that_dies_mid_job_or_talks_nonsense_is_a_lost_guest() {
    let (_dir, layout) = state_dir();
    let vmm = FakeVmm::new([
        formatter(),
        job(|mut stream, power, _| {
            // The host has taken the job as running once it answers a heartbeat with nothing: give it a moment.
            say(&mut stream, GuestMessage::Heartbeat(usage()));
            std::thread::sleep(Duration::from_millis(150));
            power.crash("the hypervisor's process died")
        }),
    ]);
    let ended = go(&vmm, &layout, request("a"), quick()).unwrap().end.unwrap();
    assert_eq!(ended.end, End::GuestLost);
    assert!(ended.message.unwrap().contains("the hypervisor's process died"));

    // The channel closes a moment before the machine is gone, as it may when the hypervisor dies: the end still
    // carries the machine's reason, not the closed channel's.
    let vmm = FakeVmm::new([job(|mut stream, power, _| {
        say(&mut stream, GuestMessage::Heartbeat(usage()));
        std::thread::sleep(Duration::from_millis(150));
        drop(stream);
        std::thread::sleep(Duration::from_millis(50));
        power.crash("the hypervisor's process died")
    })]);
    let ended = go(&vmm, &layout, request("c"), quick()).unwrap().end.unwrap();
    assert_eq!(ended.end, End::GuestLost);
    assert!(
        ended
            .message
            .as_deref()
            .is_some_and(|m| m.contains("the hypervisor's process died")),
        "{:?}",
        ended.message
    );

    let vmm = FakeVmm::new([job(|mut stream, power, _| {
        use std::io::Write;
        // A frame that claims a gigabyte.
        let _ = stream.write_all(&[0x40, 0, 0, 0, b'x']);
        power.wait_for_the_end();
    })]);
    assert_eq!(
        go(&vmm, &layout, request("b"), quick()).unwrap().end.unwrap().end,
        End::GuestLost
    );
}

#[test]
fn a_stop_over_the_socket_reaches_the_guest_with_its_reason_and_names_the_end() {
    let (_dir, layout) = state_dir();
    let (seen, stops) = mpsc::channel();
    let vmm = FakeVmm::new([
        formatter(),
        job(move |mut stream, _, _| {
            say(&mut stream, GuestMessage::Heartbeat(usage()));
            let Ok(Some(HostMessage::Stop(stop))) = read_message::<HostMessage>(&mut stream) else {
                return;
            };
            seen.send(stop.clone()).unwrap();
            finish(stream, summary(GuestEnd::Stopped(stop.reason), Some(143)));
        }),
    ]);
    // A guest that touched more than its job ever held: the Mac holds what the guest touched.
    vmm.hold(Some(4061 << 20));
    let asker = {
        let layout = layout.clone();
        std::thread::spawn(move || {
            // The daemon's side: wait until the job runs, look at it, then evict it.
            let running = loop {
                match control::ask(&layout, "a", &Request::State, Duration::from_secs(2)) {
                    Ok(state) if state.stage == Stage::Running && state.usage.is_some() => break state,
                    _ => std::thread::sleep(Duration::from_millis(10)),
                }
            };
            control::ask(
                &layout,
                "a",
                &Request::Stop {
                    reason: StopReason::EvictedMemory,
                    grace_ms: 20_000,
                },
                Duration::from_secs(2),
            )
            .unwrap();
            running
        })
    };
    let mut limits = quick();
    limits.heartbeat = Duration::from_secs(30);
    let state = go(&vmm, &layout, request("a"), limits).unwrap();
    let running = asker.join().unwrap();
    assert_eq!(
        (running.pid, running.usage, running.resident_bytes, running.end),
        (std::process::id(), Some(usage()), Some(4061 << 20), None)
    );
    assert_eq!(state.resident_bytes, None, "a machine that is gone holds nothing");
    assert_eq!(JobState::read(&layout, "a").unwrap().resident_bytes, None);
    let stop = stops.recv().unwrap();
    assert_eq!((stop.reason, stop.grace_ms), (StopReason::EvictedMemory, 20_000));
    assert_eq!(state.end.unwrap().end, End::EvictedMemory);
    assert!(
        control::ask(&layout, "a", &Request::State, Duration::from_millis(200)).is_err(),
        "the socket outlived the job"
    );
}

#[test]
fn a_guest_that_ignores_a_stop_is_stopped_by_force_also_when_its_machine_hangs() {
    let (_dir, layout) = state_dir();
    let stubborn = Script::Boot {
        refuse_channel: 0,
        guest: Box::new(|mut stream, power| {
            greet(&mut stream);
            say(&mut stream, GuestMessage::Ready { boot_ms: 1 });
            let beats = std::thread::spawn({
                let mut stream = stream.try_clone().unwrap();
                move || {
                    while write_message(&mut stream, &GuestMessage::Heartbeat(usage())).is_ok() {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                }
            });
            power.wait_for_the_end();
            drop(stream);
            let _ = beats.join();
        }),
        stop: Stop::Hangs,
    };
    let vmm = FakeVmm::new([formatter(), stubborn]);
    let cancel = AtomicBool::new(true);
    let started = std::time::Instant::now();
    let state = run(&vmm, &layout, GUEST, request("a"), quick(), &cancel).unwrap();
    assert_eq!(state.end.unwrap().end, End::Cancelled);
    assert!(vmm.stops() >= 1);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_job_at_its_deadline_is_told_and_ends_as_a_timeout() {
    let (_dir, layout) = state_dir();
    let (seen, stops) = mpsc::channel();
    let vmm = FakeVmm::new([
        formatter(),
        job(move |mut stream, _, _| {
            let Ok(Some(HostMessage::Stop(stop))) = read_message::<HostMessage>(&mut stream) else {
                return;
            };
            seen.send((stop.reason, stop.grace_ms)).unwrap();
            finish(stream, summary(GuestEnd::Stopped(stop.reason), None));
        }),
    ]);
    let mut limits = quick();
    limits.deadline = Duration::from_millis(150);
    assert_eq!(
        go(&vmm, &layout, request("a"), limits).unwrap().end.unwrap().end,
        End::Timeout
    );
    assert_eq!(stops.recv().unwrap(), (StopReason::Timeout, 100));
}

#[test]
fn a_request_no_job_can_have_starts_nothing() {
    let (_dir, layout) = state_dir();
    let vmm = FakeVmm::new([]);
    for (change, why) in [
        (
            Box::new(|r: &mut JobRequest| r.job = "../../etc".into()) as Box<dyn Fn(&mut JobRequest)>,
            "name",
        ),
        (Box::new(|r| r.job = "Job".into()), "name"),
        (Box::new(|r| r.job = String::new()), "name"),
        (Box::new(|r| r.job = "a".repeat(33)), "name"),
        (Box::new(|r| r.job = "a.b".into()), "name"),
        (Box::new(|r| r.image = "../image-1".into()), "image"),
        (Box::new(|r| r.image = "a/b".into()), "image"),
        (Box::new(|r| r.memory_bytes = (512 << 20) - 1), "memory"),
        (Box::new(|r| r.memory_bytes = u64::MAX), "memory"),
        (Box::new(|r| r.cpus = 0), "CPUs"),
        (Box::new(|r| r.scratch_bytes = 0), "disk"),
        (Box::new(|r| r.deadline_s = 0), "deadline"),
        (Box::new(|r| r.host = "a\nb".into()), "host"),
        (
            Box::new(|r| r.workload = Workload::Command { argv: vec![] }),
            "command",
        ),
    ] {
        let mut bad = request("a");
        change(&mut bad);
        let error = go(&vmm, &layout, bad, quick()).unwrap_err();
        assert!(
            matches!(error, RunError::Request(_)) && error.to_string().contains(why),
            "{error}"
        );
    }
    assert!(matches!(
        go(
            &vmm,
            &layout,
            JobRequest {
                image: "image-2".into(),
                ..request("a")
            },
            quick()
        ),
        Err(RunError::Image(_))
    ));
    assert!(vmm.started().is_empty());
    assert!(!layout.job_dir("a").exists() && !layout.state("a").exists());
}

#[test]
fn a_job_whose_name_is_taken_is_refused_and_the_first_ones_files_stay() {
    let (_dir, layout) = state_dir();
    let vmm = FakeVmm::new([
        formatter(),
        job(|stream, _, _| finish(stream, summary(GuestEnd::Exited, Some(0)))),
    ]);
    let first = go(&vmm, &layout, request("a"), quick()).unwrap();
    assert!(matches!(
        go(&vmm, &layout, request("a"), quick()),
        Err(RunError::Exists(_))
    ));
    assert_eq!(
        serde_json::from_slice::<JobState>(&std::fs::read(layout.state("a")).unwrap()).unwrap(),
        first
    );
}

#[test]
fn a_request_reads_from_json_and_a_broken_one_does_not_echo_the_registration() {
    let json = serde_json::to_string(&request("a")).unwrap();
    assert!(
        json.contains(TOKEN),
        "the request carries the registration to the vm-host"
    );
    assert_eq!(JobRequest::parse(&json).unwrap(), request("a"));
    for broken in [
        json.replace("\"cpus\":4", "\"cpus\":\"four\""),
        json[..json.len() - 1].to_string(),
        format!("{json}{json}"),
        String::new(),
    ] {
        let error = JobRequest::parse(&broken).unwrap_err().to_string();
        assert!(error.contains("malformed") && !error.contains(TOKEN), "{error}");
    }
    assert!(
        JobRequest::parse(&json.replace("\"job\":\"a\"", "\"job\":\"../a\""))
            .unwrap_err()
            .to_string()
            .contains("name")
    );
}

#[test]
fn the_socket_answers_nonsense_with_an_error_and_goes_on() {
    use std::io::{Read, Write};
    let (_dir, layout) = state_dir();
    let (release, wait) = mpsc::channel::<()>();
    let vmm = FakeVmm::new([
        formatter(),
        job(move |mut stream, _, _| {
            say(&mut stream, GuestMessage::Heartbeat(usage()));
            let _ = wait.recv_timeout(Duration::from_secs(5));
            finish(stream, summary(GuestEnd::Exited, Some(0)));
        }),
    ]);
    let prober = {
        let layout = layout.clone();
        std::thread::spawn(move || {
            while control::ask(&layout, "a", &Request::State, Duration::from_secs(1))
                .map(|state| state.stage)
                .ok()
                != Some(Stage::Running)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut answers = Vec::new();
            for nonsense in [&b"{\"cmd\":\"reboot\"}\n"[..], &[0xff; 9000][..], b""] {
                let mut stream = UnixStream::connect(layout.socket("a")).unwrap();
                let _ = stream.write_all(nonsense);
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let mut answer = String::new();
                let _ = stream.read_to_string(&mut answer);
                answers.push(answer);
            }
            let after = control::ask(&layout, "a", &Request::State, Duration::from_secs(1))
                .map(|state| state.stage)
                .ok();
            release.send(()).unwrap();
            (answers, after)
        })
    };
    let mut limits = quick();
    limits.heartbeat = Duration::from_secs(30);
    let state = go(&vmm, &layout, request("a"), limits).unwrap();
    let (answers, after) = prober.join().unwrap();
    assert!(
        answers
            .iter()
            .all(|answer| answer.trim() == r#"{"error":"not a request"}"#),
        "{answers:?}"
    );
    assert_eq!(after, Some(Stage::Running));
    assert_eq!(state.end.unwrap().end, End::Ok);
}

#[test]
fn a_guest_that_never_stops_talking_still_meets_its_deadline() {
    let (_dir, layout) = state_dir();
    let vmm = Arc::new(FakeVmm::new([
        formatter(),
        job(|stream, power, _| {
            // Heartbeats as fast as the host takes them, and no ear for a stop.
            let mut talker = stream.try_clone().unwrap();
            let talk = std::thread::spawn(move || {
                while write_message(&mut talker, &GuestMessage::Heartbeat(usage())).is_ok() {}
            });
            power.wait_for_the_end();
            let _ = stream.shutdown(std::net::Shutdown::Both);
            let _ = talk.join();
        }),
    ]));
    let mut limits = quick();
    (limits.deadline, limits.publish) = (Duration::from_millis(150), Duration::from_secs(1));
    let started = Instant::now();
    let state = go_within(&vmm, &layout, request("a"), limits, false).unwrap();
    assert_eq!(state.end.unwrap().end, End::Timeout);
    assert!(vmm.stops() >= 1, "the machine was not stopped by force");
    // The deadline, its grace and the slack: 450 ms, and the boot before them.
    assert!(
        started.elapsed() >= limits.longest_run(),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_frame_larger_than_a_guest_of_ours_sends_is_a_lost_guest_and_not_read() {
    let (_dir, layout) = state_dir();
    let vmm = Arc::new(FakeVmm::new([
        formatter(),
        job(|mut stream, power, _| {
            // A well-formed message, well under the megabyte a frame may be, that says the job failed.
            say(
                &mut stream,
                GuestMessage::Failed {
                    stage: "job".into(),
                    message: "x".repeat(100 << 10),
                },
            );
            power.wait_for_the_end();
        }),
    ]));
    let ended = go_within(&vmm, &layout, request("a"), quick(), false)
        .unwrap()
        .end
        .unwrap();
    assert_eq!(ended.end, End::GuestLost);
    assert!(std::fs::metadata(layout.state("a")).unwrap().len() < 2048);
}

#[test]
fn a_jobs_console_log_starts_empty_is_its_users_alone_and_follows_no_link() {
    let (dir, layout) = state_dir();
    let logs = layout.console("a").parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700)).unwrap();
    // What an earlier job of the name left, readable by all; and for the next job a link to a file elsewhere.
    std::fs::write(layout.console("a"), "an earlier job's lines\n").unwrap();
    std::fs::set_permissions(layout.console("a"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::write(&elsewhere, "not a log").unwrap();
    std::os::unix::fs::symlink(&elsewhere, layout.console("b")).unwrap();

    let vmm = FakeVmm::new([
        formatter(),
        job(|stream, _, _| finish(stream, summary(GuestEnd::Exited, Some(0)))),
        job(|stream, _, _| finish(stream, summary(GuestEnd::Exited, Some(0)))),
    ]);
    for name in ["a", "b"] {
        assert_eq!(
            go(&vmm, &layout, request(name), quick())
                .unwrap()
                .end
                .unwrap()
                .end,
            End::Ok
        );
        let console = layout.console(name);
        assert_eq!(std::fs::read_to_string(&console).unwrap(), "", "{name}");
        assert_eq!(mode(&console), 0o600, "{name}");
        assert!(std::fs::symlink_metadata(&console).unwrap().is_file(), "{name}");
    }
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "not a log");
}

#[test]
fn a_machine_that_fills_the_console_log_is_stopped() {
    let (_dir, layout) = state_dir();
    let console = layout.console("a");
    let vmm = Arc::new(FakeVmm::new([
        formatter(),
        job(move |mut stream, power, _| {
            say(&mut stream, GuestMessage::Heartbeat(usage()));
            std::fs::write(&console, vec![b'!'; (1 << 20) + 1]).unwrap();
            power.wait_for_the_end();
        }),
    ]));
    let mut limits = quick();
    limits.heartbeat = Duration::from_secs(60);
    let state = go_within(&vmm, &layout, request("a"), limits, false).unwrap();
    let ended = state.end.unwrap();
    assert_eq!(ended.end, End::GuestLost);
    assert!(ended.message.unwrap().contains("console"));
    assert!(vmm.stops() >= 1);
}

#[test]
fn a_console_log_of_exactly_the_limit_is_fine() {
    let (_dir, layout) = state_dir();
    let console = layout.console("a");
    let vmm = Arc::new(FakeVmm::new([
        formatter(),
        job(move |stream, _, _| {
            std::fs::write(&console, vec![b'!'; 1 << 20]).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            finish(stream, summary(GuestEnd::Exited, Some(0)));
        }),
    ]));
    let mut limits = quick();
    limits.heartbeat = Duration::from_secs(60);
    let state = go_within(&vmm, &layout, request("a"), limits, false).unwrap();
    assert_eq!(state.end.unwrap().end, End::Ok);
}

#[test]
fn a_socket_that_cannot_be_made_starts_no_job_and_leaves_the_name_free() {
    let (_dir, layout) = state_dir();
    // Something that is no socket and cannot be removed as a file sits where the socket goes.
    std::fs::create_dir_all(layout.socket("a").join("in the way")).unwrap();
    let vmm = Arc::new(FakeVmm::new([
        formatter(),
        job(|stream, _, _| finish(stream, summary(GuestEnd::Exited, Some(0)))),
    ]));
    let error = go_within(&vmm, &layout, request("a"), quick(), false).unwrap_err();
    assert!(matches!(error, RunError::Io(..)), "{error}");
    assert!(vmm.started().is_empty());
    assert!(!layout.state("a").exists() && !layout.job_dir("a").exists());

    std::fs::remove_dir_all(layout.socket("a")).unwrap();
    let state = go_within(&vmm, &layout, request("a"), quick(), false).unwrap();
    assert_eq!(state.end.unwrap().end, End::Ok);
}

/// A machine that starts formatting a disk and never comes to an end.
fn stuck_formatter() -> Script {
    Script::boot(|mut stream, power| {
        greet(&mut stream);
        say(&mut stream, GuestMessage::Ready { boot_ms: 1 });
        power.wait_for_the_end();
    })
}

#[test]
fn a_stop_while_the_disk_is_made_ends_the_job_without_waiting_for_the_disk() {
    // A signal.
    let (_dir, layout) = state_dir();
    let vmm = Arc::new(FakeVmm::new([stuck_formatter()]));
    let started = Instant::now();
    let state = go_within(&vmm, &layout, request("a"), quick(), true).unwrap();
    let ended = state.end.clone().unwrap();
    assert_eq!(
        (state.stage, ended.end, state.boots),
        (Stage::Ended, End::Cancelled, 0)
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        serde_json::from_slice::<JobState>(&std::fs::read(layout.state("a")).unwrap()).unwrap(),
        state
    );
    assert!(vmm.stops() >= 1, "the formatting machine was left running");
    assert!(!layout.job_dir("a").exists() && !layout.socket("a").exists());
    let left: Vec<_> = std::fs::read_dir(layout.scratch_templates())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect();
    assert_eq!(left, ["image-1-1073741824.lock"]);

    // A stop over the socket, with its own reason.
    let (_dir, layout) = state_dir();
    let vmm = Arc::new(FakeVmm::new([stuck_formatter()]));
    let asker = {
        let layout = layout.clone();
        std::thread::spawn(move || {
            let stop = Request::Stop {
                reason: StopReason::EvictedDisk,
                grace_ms: 60_000,
            };
            let asked = Instant::now();
            while control::ask(&layout, "a", &stop, Duration::from_secs(1)).is_err() {
                assert!(asked.elapsed() < Duration::from_secs(8), "no vm-host answered");
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let state = go_within(&vmm, &layout, request("a"), quick(), false).unwrap();
    asker.join().unwrap();
    assert_eq!((state.end.unwrap().end, state.boots), (End::EvictedDisk, 0));

    // Another vm-host makes the same disk and takes its time: the wait for it ends with the stop too.
    let (_dir, layout) = state_dir();
    std::fs::create_dir_all(layout.scratch_templates()).unwrap();
    let held = std::fs::File::create(layout.scratch_templates().join("image-1-1073741824.lock")).unwrap();
    // SAFETY: flock takes no pointers; the descriptor is open for the call.
    assert_eq!(
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&held), libc::LOCK_EX) },
        0
    );
    let vmm = Arc::new(FakeVmm::new([]));
    let state = go_within(&vmm, &layout, request("a"), quick(), true).unwrap();
    assert_eq!(state.end.unwrap().end, End::Cancelled);
    assert!(vmm.started().is_empty());
}

#[test]
fn one_slow_client_does_not_keep_the_socket_from_the_next() {
    use std::io::Write;
    let (_dir, layout) = state_dir();
    let (release, wait) = mpsc::channel::<()>();
    let vmm = Arc::new(FakeVmm::new([
        formatter(),
        job(move |mut stream, _, _| {
            say(&mut stream, GuestMessage::Heartbeat(usage()));
            let _ = wait.recv_timeout(Duration::from_secs(8));
            finish(stream, summary(GuestEnd::Exited, Some(0)));
        }),
    ]));
    let prober = {
        let layout = layout.clone();
        std::thread::spawn(move || {
            while control::ask(&layout, "a", &Request::State, Duration::from_secs(1))
                .map(|state| state.stage)
                .ok()
                != Some(Stage::Running)
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            // A client that sends a byte every 300 ms: never silent for the second a read waits.
            let mut slow = UnixStream::connect(layout.socket("a")).unwrap();
            let trickle = std::thread::spawn(move || {
                for byte in br#"{"cmd":"state"}"# {
                    if slow.write_all(&[*byte]).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
            std::thread::sleep(Duration::from_millis(100));
            let asked = Instant::now();
            let answer = control::ask(&layout, "a", &Request::State, Duration::from_millis(2500))
                .map(|state| state.stage)
                .ok();
            let waited = asked.elapsed();
            release.send(()).unwrap();
            let _ = trickle.join();
            (answer, waited)
        })
    };
    let mut limits = quick();
    limits.heartbeat = Duration::from_secs(60);
    let state = go_within(&vmm, &layout, request("a"), limits, false).unwrap();
    let (answer, waited) = prober.join().unwrap();
    assert_eq!(answer, Some(Stage::Running), "after {waited:?}");
    assert!(waited < Duration::from_millis(1500), "{waited:?}");
    assert_eq!(state.end.unwrap().end, End::Ok);
}

#[test]
fn what_a_job_leaves_is_this_users_alone_and_a_folder_others_can_write_to_boots_nothing() {
    let (dir, layout) = state_dir();
    let quiet = || job(|stream, _, _| finish(stream, summary(GuestEnd::Exited, Some(0))));
    // What is there only while the job runs: its socket and its machine's files, the job's disk among them.
    let (seen, while_running) = mpsc::channel();
    let looks = {
        let layout = layout.clone();
        job(move |stream, _, _| {
            let mut modes = vec![("the socket".to_string(), mode(layout.socket("a")))];
            for entry in std::fs::read_dir(layout.job_dir("a")).unwrap() {
                let entry = entry.unwrap();
                modes.push((
                    entry.file_name().to_string_lossy().into_owned(),
                    mode(entry.path()),
                ));
            }
            modes.sort();
            seen.send(modes).unwrap();
            finish(stream, summary(GuestEnd::Exited, Some(0)));
        })
    };
    let vmm = Arc::new(FakeVmm::new([formatter(), looks]));
    go_within(&vmm, &layout, request("a"), quick(), false).unwrap();
    assert_eq!(
        while_running.recv().unwrap(),
        [
            ("initramfs".to_string(), 0o600),
            ("kernel".to_string(), 0o600),
            ("scratch.img".to_string(), 0o600),
            ("the socket".to_string(), 0o600),
        ]
    );
    for kept in ["image-1-1073741824.img", "image-1-1073741824.lock"] {
        assert_eq!(mode(layout.scratch_templates().join(kept)), 0o600, "{kept}");
    }
    assert_eq!(mode(layout.state("a")), 0o600);
    assert_eq!(mode(layout.run()), 0o700);
    assert_eq!(mode(layout.scratch_templates()), 0o700);
    assert_eq!(mode(layout.console("a").parent().unwrap()), 0o700);

    let rootfs = layout.rootfs("image-1");
    let image = rootfs.parent().unwrap();
    let images = image.parent().unwrap();
    for (path, open, private) in [
        (rootfs.as_path(), 0o666, 0o644),
        (rootfs.as_path(), 0o664, 0o644),
        (image, 0o777, 0o755),
        (images, 0o775, 0o700),
        (dir.path(), 0o757, 0o700),
        (layout.run().as_path(), 0o770, 0o700),
        (layout.scratch_templates().as_path(), 0o777, 0o700),
    ] {
        let set = |mode| std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        set(open);
        let vmm = Arc::new(FakeVmm::new([quiet()]));
        let error = go_within(&vmm, &layout, request("b"), quick(), false).unwrap_err();
        set(private);
        assert!(
            matches!(error, RunError::NotPrivate(_) | RunError::Scratch(_))
                && error.to_string().contains("written by other users")
                && error.to_string().contains(&path.display().to_string()),
            "{}: {error}",
            path.display()
        );
        assert!(vmm.started().is_empty(), "{} booted a machine", path.display());
        assert!(!layout.state("b").exists() && !layout.job_dir("b").exists());
    }
    // And with everything private again the job runs.
    let vmm = Arc::new(FakeVmm::new([quiet()]));
    assert_eq!(
        go_within(&vmm, &layout, request("b"), quick(), false)
            .unwrap()
            .end
            .unwrap()
            .end,
        End::Ok
    );
}

#[test]
fn a_vm_host_answers_for_its_own_state_and_no_other() {
    let (_dir, layout) = state_dir();
    let (release, wait) = mpsc::channel::<()>();
    let vmm = Arc::new(FakeVmm::new([
        formatter(),
        job(move |mut stream, _, _| {
            say(&mut stream, GuestMessage::Heartbeat(usage()));
            let _ = wait.recv_timeout(Duration::from_secs(8));
            finish(stream, summary(GuestEnd::Exited, Some(0)));
        }),
    ]));
    let daemon = {
        let layout = layout.clone();
        std::thread::spawn(move || {
            let read = || -> Option<JobState> {
                serde_json::from_slice(&std::fs::read(layout.state("a")).ok()?).ok()
            };
            let state = loop {
                match read() {
                    Some(state) if state.stage == Stage::Running => break state,
                    _ => std::thread::sleep(Duration::from_millis(10)),
                }
            };
            let within = Duration::from_secs(1);
            let own = control::answers(&layout, &state, within);
            // The same file read by a daemon after the vm-host's number went to another process.
            let reused = control::answers(
                &layout,
                &JobState {
                    pid: state.pid + 1,
                    ..state.clone()
                },
                within,
            );
            let other_job = control::answers(
                &layout,
                &JobState {
                    job: "b".into(),
                    ..state.clone()
                },
                within,
            );
            release.send(()).unwrap();
            (state, own, reused, other_job)
        })
    };
    let mut limits = quick();
    limits.heartbeat = Duration::from_secs(60);
    go_within(&vmm, &layout, request("a"), limits, false).unwrap();
    let (state, own, reused, other_job) = daemon.join().unwrap();
    assert_eq!((own, reused, other_job), (true, false, false));
    assert!(
        !control::answers(&layout, &state, Duration::from_millis(200)),
        "a vm-host that is gone answered"
    );
}

#[test]
fn a_machine_that_takes_no_stop_is_let_go_of_at_once() {
    let (_dir, layout) = state_dir();
    let cancel = Arc::new(AtomicBool::new(false));
    let deaf = Script::Boot {
        refuse_channel: 0,
        guest: Box::new({
            let cancel = cancel.clone();
            move |mut stream, power| {
                greet(&mut stream);
                say(&mut stream, GuestMessage::Ready { boot_ms: 1 });
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                power.wait_for_the_end();
            }
        }),
        stop: Stop::Refuses,
    };
    let vmm = FakeVmm::new([formatter(), deaf]);
    // A machine that only hangs in its stop is waited for this long; one that refuses it is not.
    let mut limits = quick();
    limits.kill = Duration::from_secs(8);
    let started = std::time::Instant::now();
    let state = run(&vmm, &layout, GUEST, request("a"), limits, &cancel).unwrap();
    assert_eq!(state.end.unwrap().end, End::Cancelled);
    assert_eq!(state.boots, 1);
    assert_eq!(vmm.stops(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn what_a_guest_says_of_its_failure_reaches_the_state_short_and_clean() {
    let (_dir, layout) = state_dir();
    let vmm = FakeVmm::new([
        formatter(),
        job(|mut stream, power, _| {
            say(
                &mut stream,
                GuestMessage::Failed {
                    stage: "s".repeat(1000),
                    message: format!("gone\n\u{1b}[2J{}", "é".repeat(20_000)),
                },
            );
            power.wait_for_the_end();
        }),
    ]);
    let ended = go(&vmm, &layout, request("a"), quick()).unwrap().end.unwrap();
    assert_eq!(ended.end, End::Failed);
    let message = ended.message.unwrap();
    assert!(
        message.starts_with(&format!("the guest failed at {}: gone  [2Jé", "s".repeat(32))),
        "{}",
        &message[..80]
    );
    assert!(message.chars().count() < 1100 && message.chars().all(|c| !c.is_control()));
    let on_disk = std::fs::read_to_string(layout.state("a")).unwrap();
    assert!(on_disk.len() < 4096, "{}", on_disk.len());
}

#[test]
fn a_request_names_an_image_by_any_version_the_store_takes() {
    for (version, fine) in [
        ("2026.10.10-1", true),
        ("2026.10.10-RC1", true),
        ("A", true),
        (&*"v".repeat(64), true),
        (&*"v".repeat(65), false),
        ("", false),
        (".hidden", false),
        ("..", false),
        ("a/b", false),
        ("a b", false),
        ("a\n", false),
        ("é", false),
    ] {
        let mut asked = request("a");
        asked.image = version.into();
        assert_eq!(asked.check().is_ok(), fine, "{version:?}");
        assert_eq!(cf_image::store::is_version(version), fine, "{version:?}");
    }
}
