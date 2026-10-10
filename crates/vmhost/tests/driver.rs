//! A whole job against a scripted machine: what the vm-host tells the guest, what it leaves on disk, and how it
//! ends the ways a job goes wrong.

use cf_guest_proto::{
    GuestEnd, GuestMessage, HostMessage, PROTOCOL, Secret, StopReason, Summary, Task, Usage, Workload,
    read_message, write_message,
};
use cf_vmhost::control::{self, Request};
use cf_vmhost::{End, Guest, JobRequest, JobState, Layout, Limits, RunError, Stage, VM_OVERHEAD, run};
use cf_vmm::testing::{FakeVmm, PowerSwitch, Script};
use cf_vmm::{Network, VmmError};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Duration;

const TOKEN: &str = "jit-registration-do-not-leak-7f3a";
const GUEST: Guest<'static> = Guest {
    kernel: b"kernel",
    initramfs: b"initramfs",
};

fn request(job: &str) -> JobRequest {
    JobRequest {
        job: job.into(),
        host: "mini".into(),
        memory_bytes: 4 << 30,
        cpus: 4,
        scratch_bytes: 1 << 30,
        image: "image-1".into(),
        workload: Workload::ActionsRunner {
            jit_config: Secret::new(TOKEN),
        },
        env: vec![("CI".into(), "true".into())],
        deadline_s: 3600,
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
        ("job-1", "mini", 15_000)
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
    // What stays is the job's state and the empty disk the next job of this image and size starts from.
    let template = layout.scratch_templates().join("image-1-1073741824.img");
    assert_eq!(std::fs::metadata(template).unwrap().len(), 1 << 30);
    let mut kept: Vec<_> = files
        .iter()
        .map(|(name, _)| name.rsplit('/').next().unwrap())
        .collect();
    kept.sort_unstable();
    assert_eq!(kept, ["image-1-1073741824.lock", "job-1.json", "rootfs.img"]);
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
        obeys_stop: true,
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
        (running.pid, running.usage, running.end),
        (std::process::id(), Some(usage()), None)
    );
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
        obeys_stop: false,
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
