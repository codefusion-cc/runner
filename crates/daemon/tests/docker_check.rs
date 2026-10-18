//! The Docker check on the machines' side: one small machine with Docker, through the budget, whose end says whether
//! Docker starts in this host's job machines; and what the check's answer refuses.

mod support;

use cf_budget::Size;
use cf_daemon::docker_check::{DockerCheck, RECHECK_AFTER};
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
