//! The Docker check on the machines' side: one small machine with Docker, through the budget, whose end says whether
//! Docker starts in this host's job machines; and what the check's answer refuses.

mod support;

use cf_budget::Size;
use cf_daemon::docker_check::{AGAIN_AFTER_NO_ANSWER, DockerCheck, RECHECK_AFTER};
use cf_daemon::names::job_name;
use cf_daemon::testing::pass;
use cf_guest_proto::Workload;
use cf_vmhost::End;
use support::world::World;

const SANDBOX: &str = "cf-supervisor: the Docker sandbox ended: exit status: 70";

/// Boots the check's machine and says its handle.
fn probe(w: &World) -> String {
    let runner =
        w.m.start_docker_probe(Size::G2)
            .expect("the check's machine starts");
    job_name(&runner).to_string()
}

#[tokio::test]
async fn the_check_takes_a_slot_with_docker_and_gives_it_back_saying_it_passed() {
    let mut w = World::new(2);
    let job = probe(&w);
    assert_eq!(w.slots(), 1, "the check took no slot");
    assert!(w.m.probing());
    let request = w.parts.vms.started().pop().expect("a machine");
    assert!(request.docker, "the check's machine has no Docker");
    assert_eq!(request.memory_bytes, Size::G2.vm_bytes());
    assert!(request.env.is_empty());
    let Workload::Command { argv } = &request.workload else {
        panic!("not a command")
    };
    assert!(argv[2].contains("docker info"), "{argv:?}");
    // Up, it is not taken for an idle Actions runner: nothing goes to GitHub and nothing stops it.
    pass(&w.m, 15 * 60);
    assert!(w.m.registrations_to_remove().is_empty());
    assert!(w.parts.vms.stopped().is_empty());
    assert_eq!(w.m.docker_check(), None, "an answer before the check ended");

    w.parts.vms.end(&job, End::Ok);
    pass(&w.m, 1);
    let check = w.m.docker_check().expect("an answer");
    assert!(check.ok && check.why.is_none(), "{check:?}");
    assert_eq!(w.slots(), 0, "the check's slot was kept");
    assert!(!w.m.probing());
    assert!(w.reports().is_empty(), "the check was reported as a job");
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_check_whose_daemon_never_answers_fails_with_its_exit_and_the_sandboxs_end() {
    let w = World::new(2);
    let job = probe(&w);
    pass(&w.m, 1);
    w.parts.vms.console(
        &job,
        &format!("boot\n{SANDBOX}\nCannot connect to the Docker daemon\n"),
    );
    w.parts.vms.end(&job, End::Failed);
    pass(&w.m, 1);
    let check = w.m.docker_check().expect("an answer");
    assert!(!check.ok);
    let why = check.why.expect("why");
    assert!(why.contains("exit 1") && why.contains(SANDBOX), "{why}");
    assert_eq!(w.slots(), 0);

    // A machine that never boots fails the check too, with what the vm-host said.
    let w = World::new(2);
    let job = probe(&w);
    w.parts.vms.end(&job, End::BootFailed);
    pass(&w.m, 1);
    let check = w.m.docker_check().expect("an answer");
    assert!(
        !check.ok && check.why.as_deref().is_some_and(|why| why.contains("boot")),
        "{check:?}"
    );
}

/// Seconds since 1970 on the Runner's wall clock.
fn wall(w: &World) -> i64 {
    let now = w
        .parts
        .clock
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970");
    i64::try_from(now.as_secs()).expect("seconds")
}

#[tokio::test]
async fn a_check_the_host_or_someone_ends_first_answers_nothing_and_boots_again_after_ten_minutes() {
    let after = i64::try_from(AGAIN_AFTER_NO_ANSWER.as_secs()).expect("seconds");
    for end in [
        End::HostSlept,
        End::EvictedMemory,
        End::EvictedDisk,
        End::Cancelled,
    ] {
        let w = World::new(2);
        assert!(w.m.docker_check_due(wall(&w)));
        let job = probe(&w);
        pass(&w.m, 1);
        // Even with the sandbox's line in its log: the machine did not end by its workload.
        w.parts.vms.console(&job, &format!("{SANDBOX}\n"));
        w.parts.vms.end(&job, end);
        pass(&w.m, 1);
        assert_eq!(w.m.docker_check(), None, "{end:?} answered");
        assert!(!w.m.probing());
        assert_eq!(w.slots(), 0, "{end:?}: the check's slot was kept");
        let now = wall(&w);
        assert!(!w.m.docker_check_due(now), "{end:?}: due again at once");
        assert!(
            !w.m.docker_check_due(now + after - 1),
            "{end:?}: due again too soon"
        );
        assert!(w.m.docker_check_due(now + after), "{end:?}: never due again");
    }

    // A failure stands while a later check is ended so.
    let w = World::new(2);
    let job = probe(&w);
    pass(&w.m, 1);
    w.parts.vms.end(&job, End::Failed);
    pass(&w.m, 1);
    let failed = w.m.docker_check().expect("an answer");
    assert!(!failed.ok);
    let job = probe(&w);
    pass(&w.m, 1);
    w.parts.vms.end(&job, End::HostSlept);
    pass(&w.m, 1);
    assert_eq!(
        w.m.docker_check(),
        Some(failed.clone()),
        "a slept check changed the answer"
    );
    let recheck = i64::try_from(RECHECK_AFTER.as_secs()).expect("seconds");
    assert!(!w.m.docker_check_due(failed.at + recheck - 1));
    assert!(w.m.docker_check_due(failed.at + recheck));
}

#[tokio::test]
async fn no_check_boots_on_a_host_that_takes_no_new_job() {
    let w = World::new(2);
    w.m.pause(true);
    assert!(w.m.start_docker_probe(Size::G2).is_err());
    w.m.pause(false);
    w.m.hold_for_restart(true);
    assert!(w.m.start_docker_probe(Size::G2).is_err());
    w.m.hold_for_restart(false);
    assert!(w.parts.vms.started().is_empty());
    assert_eq!(w.slots(), 0);
    // Nor on a full one: it waits for a slot like any claim.
    let w = World::new(1);
    let _first = probe(&w);
    assert!(w.m.start_docker_probe(Size::G2).is_err());
    assert_eq!(w.slots(), 1);
}

#[tokio::test]
async fn a_runner_that_starts_again_adopts_the_check_and_hears_its_answer() {
    let mut w = World::new(2);
    let job = probe(&w);
    pass(&w.m, 1);
    w.restart();
    pass(&w.m, 1);
    assert_eq!(w.slots(), 1);
    assert!(w.m.probing());
    assert!(w.m.registrations_to_remove().is_empty());
    w.parts.vms.end(&job, End::Ok);
    pass(&w.m, 1);
    assert!(w.m.docker_check().is_some_and(|check| check.ok));
    assert!(w.reports().is_empty());
    w.assert_nothing_left();
}

#[test]
fn unknown_and_passed_refuse_nothing_a_failure_refuses_until_it_passes_and_is_checked_again_after_six_hours()
{
    let after = i64::try_from(RECHECK_AFTER.as_secs()).expect("seconds");
    assert_eq!(DockerCheck::refusal(None), None);
    assert!(DockerCheck::due(None, 0));
    let passed = DockerCheck {
        ok: true,
        at: 100,
        why: None,
    };
    assert_eq!(DockerCheck::refusal(Some(&passed)), None);
    assert!(!DockerCheck::due(Some(&passed), 100 + 10 * after));
    let failed = DockerCheck {
        ok: false,
        at: 100,
        why: Some("exit 1".into()),
    };
    assert_eq!(
        DockerCheck::refusal(Some(&failed)).as_deref(),
        Some("Docker does not start in this machine's job VMs (exit 1)")
    );
    assert!(!DockerCheck::due(Some(&failed), 100 + after - 1));
    assert!(DockerCheck::due(Some(&failed), 100 + after));
}

/// Fails the check on this host: its machine boots and `docker info` never answers.
fn fail_check(w: &World) {
    let job = probe(w);
    pass(&w.m, 1);
    w.parts.vms.console(&job, &format!("{SANDBOX}\n"));
    w.parts.vms.end(&job, End::Failed);
    pass(&w.m, 1);
    assert!(w.m.docker_check().is_some_and(|check| !check.ok));
}

#[tokio::test]
async fn a_failed_check_holds_across_a_restart_until_a_new_one_passes() {
    let mut w = World::new(2);
    fail_check(&w);
    let failed = w.m.docker_check().expect("an answer");
    w.restart();
    // Before any new check ends, the Runner that starts again refuses Docker for what the one before found.
    assert_eq!(
        w.m.docker_check(),
        Some(failed.clone()),
        "the failure was forgotten"
    );
    assert!(DockerCheck::refusal(w.m.docker_check().as_ref()).is_some());
    // The calm minute a Runner that starts waits for, then the check again.
    pass(&w.m, 61);
    let job = probe(&w);
    pass(&w.m, 1);
    w.parts.vms.end(&job, End::Ok);
    pass(&w.m, 1);
    assert!(w.m.docker_check().is_some_and(|check| check.ok));
    // A pass is not kept: the next start checks again, and Docker is unknown meanwhile.
    w.restart();
    assert_eq!(w.m.docker_check(), None, "a pass was kept across a restart");
}

#[test]
fn a_failed_check_is_kept_on_disk_and_an_unreadable_one_is_unknown() {
    use cf_daemon::{DiskStore, Store};
    let folder = tempfile::tempdir().expect("folder");
    let store = DiskStore::new(folder.path()).expect("store");
    assert_eq!(store.docker_failure(), None);
    let failed = DockerCheck {
        ok: false,
        at: 1_790_000_000,
        why: Some(format!(
            "docker info did not answer in 2 minutes (exit 1); {SANDBOX}"
        )),
    };
    store.keep_docker_failure(Some(&failed));
    let again = DiskStore::new(folder.path()).expect("store");
    assert_eq!(again.docker_failure(), Some(failed));
    assert!(again.load().is_empty(), "the check was read as a job's record");
    again.keep_docker_failure(None);
    assert_eq!(again.docker_failure(), None);
    again.keep_docker_failure(None);
    for corrupt in [&b"{"[..], b"[]", b"{\"ok\": true, \"at\": 1}", &[0xff, 0xfe]] {
        let path = std::fs::read_dir(folder.path())
            .expect("folder")
            .flatten()
            .map(|entry| entry.path())
            .next();
        assert!(path.is_none(), "{path:?} is left");
        std::fs::write(folder.path().join("docker-check.failed"), corrupt).expect("written");
        assert_eq!(again.docker_failure(), None, "{corrupt:?} was read as a failure");
        std::fs::remove_file(folder.path().join("docker-check.failed")).expect("removed");
    }
}
