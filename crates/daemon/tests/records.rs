//! Records an older Runner left, read by this one ([`Record::upgrade`]). Every combination of what the Runner of
//! main (`d8f202c`) and the builds of this branch before these rules could have written is read: each goes on as
//! consistent under these rules or asks GitHub, and none is stopped or forgotten on a belief.

mod support;

use cf_daemon::names::job_name;
use cf_daemon::testing::pass;
use cf_daemon::{RULES, Record, Store, Upgrade, never_up};
use cf_lifecycle::{End, Phase};
use serde_json::{Value, json};
use support::world::World;

const PHASES: [Phase; 9] = [
    Phase::Reserved,
    Phase::Starting,
    Phase::Retrying,
    Phase::Idle,
    Phase::Running,
    Phase::Finishing,
    Phase::Stopping,
    Phase::Killing,
    Phase::Released,
];

const ENDS: [End; 11] = [
    End::Ok,
    End::Oom,
    End::EvictedMemory,
    End::EvictedDisk,
    End::Timeout,
    End::GuestLost,
    End::HostSlept,
    End::Stopped,
    End::Lost,
    End::StartFailed,
    End::NoJob,
];

/// What GitHub had said of the job.
#[derive(Clone, Copy, Debug)]
enum Word {
    None,
    Started,
    Completed,
}

/// Which Runner wrote the record, by the fields it had.
#[derive(Clone, Copy, Debug)]
enum Writer {
    /// main: neither `ask_github` nor `removing` nor `rules`.
    Main,
    /// This branch before these rules, with `removing` (what it meant to do) and `ask_github`.
    Removing { ask_github: bool, removing: bool },
    /// This branch's round before, with `ask_github` alone.
    Asking { ask_github: bool },
}

#[derive(Clone, Copy, Debug)]
struct Old {
    phase: Phase,
    end: Option<End>,
    word: Word,
    ran_job: bool,
    unregistered: bool,
    ended: Option<&'static str>,
    end_reported: bool,
    writer: Writer,
}

/// The record as that Runner wrote it to disk.
fn written(old: Old) -> Record {
    let index = |phase: Phase| PHASES.iter().position(|p| *p == phase).expect("a phase");
    let github = |result: Value| {
        json!({"repo": "org/app", "job": "build", "workflow": "ci.yml", "run_id": 42, "event": "push",
               "queued_at": 0, "started_at": 0, "result": result, "completed_at": null})
    };
    let mut record = json!({
        "job": "j0000001", "runner": "mac-b-j0000001", "runner_id": 1, "scale_set_id": 7, "repo": "org/app",
        "size": "cf-2g", "memory_bytes": 2u64 << 30, "limit_bytes": 1u64 << 30, "cpus": 2, "origin": "claimed",
        "slot": {
            "phase": index(old.phase),
            "starts": 1,
            "end": old.end.map(|end| ENDS.iter().position(|e| *e == end).expect("an end")),
            "oom": false,
            "ran_job": old.ran_job,
        },
        "created_at": 0, "boot": "start-1", "unregistered": old.unregistered,
        "github": match old.word {
            Word::None => Value::Null,
            Word::Started => github(Value::Null),
            Word::Completed => github(json!("succeeded")),
        },
        "start_reported": !matches!(old.word, Word::None),
        "ended": old.ended.map(|kind| json!({"kind": kind, "at": 0, "boot_ms": null, "memory_peak_bytes": null,
            "disk_peak_bytes": null, "cpu_usec": null, "wall_ms": null, "message": null})),
        "end_reported": old.end_reported,
        "oom": false,
    });
    match old.writer {
        Writer::Main => {}
        Writer::Removing { ask_github, removing } => {
            record["ask_github"] = json!(ask_github);
            record["removing"] = if removing { json!("surplus") } else { Value::Null };
        }
        Writer::Asking { ask_github } => record["ask_github"] = json!(ask_github),
    }
    serde_json::from_value(record).expect("a record an older Runner wrote is read")
}

fn all() -> impl Iterator<Item = Old> {
    let ends = std::iter::once(None).chain(ENDS.map(Some));
    let writers = [
        Writer::Main,
        Writer::Removing {
            ask_github: false,
            removing: false,
        },
        Writer::Removing {
            ask_github: false,
            removing: true,
        },
        Writer::Removing {
            ask_github: true,
            removing: false,
        },
        Writer::Removing {
            ask_github: true,
            removing: true,
        },
        Writer::Asking { ask_github: false },
        Writer::Asking { ask_github: true },
    ];
    let mut every = Vec::new();
    for phase in PHASES {
        for end in ends.clone() {
            for word in [Word::None, Word::Started, Word::Completed] {
                for ran_job in [false, true] {
                    for unregistered in [false, true] {
                        for ended in [None, Some("ok"), Some("no-job"), Some("stopped")] {
                            for end_reported in [false, true] {
                                for writer in writers {
                                    every.push(Old {
                                        phase,
                                        end,
                                        word,
                                        ran_job,
                                        unregistered,
                                        ended,
                                        end_reported,
                                        writer,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    every.into_iter()
}

/// What these rules forbid to follow from a record an older Runner wrote.
fn acted_on_belief(before: &Record, after: &Record, upgrade: Upgrade) -> Option<&'static str> {
    let saved = after.slot.expect("a slot").saved();
    if upgrade == Upgrade::Current {
        return Some("trusted as written under these rules");
    }
    if after.rules != RULES || !after.agrees() {
        return Some("left with facts that disagree");
    }
    // A machine gone before its guest came up took no job: what was written of it goes on.
    let written = before.slot.expect("a slot").saved();
    let gone_never_up =
        before.github.is_none() && before.ended.is_some() && !written.ran_job && never_up(written);
    if after.github.is_none() && !gone_never_up {
        if after.unregistered {
            return Some("an older Runner's `unregistered` with no job taken as GitHub's 204");
        }
        if after.finished() {
            return Some("forgotten with no word of a job");
        }
        if after.ended.is_some() && !(after.ask_github && !after.end_reported) {
            return Some("a machine gone with no word of a job is not asked about");
        }
    }
    if gone_never_up && (after.ask_github || after.unregistered != before.unregistered) {
        return Some("a machine whose guest never came up asked about");
    }
    let standing = after.ended.is_none();
    let stopping = matches!(saved.phase, Phase::Stopping | Phase::Killing);
    if standing && after.github.is_none() && stopping && matches!(saved.end, Some(End::NoJob | End::Stopped))
    {
        return Some("a standing machine stopped for no job before GitHub was asked");
    }
    if before.github != after.github {
        return Some("GitHub's word changed");
    }
    let asks = upgrade == Upgrade::AskGithub;
    if asks && standing && saved.phase == Phase::Idle && saved.end != Some(End::NoJob) {
        return Some("asks with a machine that is not asking");
    }
    None
}

#[test]
fn every_record_an_older_runner_could_write_goes_on_consistent_or_asks_github() {
    let mut classes = [0usize; 2];
    for old in all() {
        let before = written(old);
        assert_eq!(before.rules, 0, "an older Runner wrote no rules");
        let (after, upgrade) = before.clone().upgrade();
        if let Some(wrong) = acted_on_belief(&before, &after, upgrade) {
            panic!("{wrong}: {old:?}\nread as {upgrade:?}: {after:?}");
        }
        classes[usize::from(upgrade == Upgrade::AskGithub)] += 1;
        // Read once: what it was made into is a record of these rules.
        assert_eq!(after.clone().upgrade(), (after, Upgrade::Current), "{old:?}");
    }
    assert!(classes.iter().all(|n| *n > 0), "{classes:?}");
}

/// Each write of main's that the rules of now read differently, by where main made it.
#[test]
fn what_main_wrote_is_read_by_these_rules() {
    let main = |phase, end, word, unregistered, ended, end_reported| Old {
        phase,
        end,
        word,
        ran_job: !matches!(word, Word::None) && phase != Phase::Starting && phase != Phase::Idle,
        unregistered,
        ended,
        end_reported,
        writer: Writer::Main,
    };
    use Word::{Completed, None as Unheard, Started};
    let rows: [(&str, Old, Upgrade, Phase); 14] = [
        (
            "`start`: a machine boots",
            main(Phase::Starting, None, Unheard, false, None, false),
            Upgrade::Consistent,
            Phase::Starting,
        ),
        (
            "`Events::job` died between its two writes, booting",
            main(Phase::Starting, None, Started, false, None, false),
            Upgrade::Consistent,
            Phase::Running,
        ),
        (
            "`Events::job` died between its two writes, idle",
            main(Phase::Idle, None, Started, false, None, false),
            Upgrade::Consistent,
            Phase::Running,
        ),
        (
            "`Events::job` died between the completion and its step",
            main(Phase::Running, None, Completed, false, None, false),
            Upgrade::Consistent,
            Phase::Finishing,
        ),
        (
            "the idle limit stopped it for no job",
            main(Phase::Stopping, Some(End::NoJob), Unheard, false, None, false),
            Upgrade::AskGithub,
            Phase::Idle,
        ),
        (
            "the idle limit's stop, then the kill",
            main(Phase::Killing, Some(End::NoJob), Unheard, false, None, false),
            Upgrade::AskGithub,
            Phase::Idle,
        ),
        (
            "`retire_surplus` stopped it after a 204 or a 404",
            main(Phase::Stopping, Some(End::Stopped), Unheard, false, None, false),
            Upgrade::AskGithub,
            Phase::Idle,
        ),
        (
            "an eviction stopped an idle machine",
            main(
                Phase::Stopping,
                Some(End::EvictedMemory),
                Unheard,
                false,
                None,
                false,
            ),
            Upgrade::Consistent,
            Phase::Stopping,
        ),
        (
            "`ended` with no word of a job",
            main(
                Phase::Released,
                Some(End::GuestLost),
                Unheard,
                false,
                Some("guest-lost"),
                false,
            ),
            Upgrade::AskGithub,
            Phase::Released,
        ),
        (
            "`settle` reported nothing and asked nothing",
            main(
                Phase::Released,
                Some(End::NoJob),
                Unheard,
                false,
                Some("no-job"),
                true,
            ),
            Upgrade::AskGithub,
            Phase::Released,
        ),
        (
            "`unregistered` after the sweep's 404",
            main(Phase::Released, Some(End::Ok), Unheard, true, Some("ok"), false),
            Upgrade::AskGithub,
            Phase::Released,
        ),
        (
            "`unregistered` and `settle`, died before the removal",
            main(Phase::Released, Some(End::Ok), Unheard, true, Some("ok"), true),
            Upgrade::AskGithub,
            Phase::Released,
        ),
        (
            "a job heard, its end not yet",
            main(
                Phase::Released,
                Some(End::GuestLost),
                Started,
                false,
                Some("guest-lost"),
                false,
            ),
            Upgrade::Consistent,
            Phase::Released,
        ),
        (
            "a job heard, both reported, died before the removal",
            main(Phase::Released, Some(End::Ok), Completed, true, Some("ok"), true),
            Upgrade::Consistent,
            Phase::Released,
        ),
    ];
    for (row, old, class, phase) in rows {
        let before = written(old);
        let (after, upgrade) = before.clone().upgrade();
        assert_eq!(
            (upgrade, after.slot.expect("a slot").saved().phase),
            (class, phase),
            "{row}"
        );
        assert_eq!(acted_on_belief(&before, &after, upgrade), None, "{row}");
    }
}

/// A job heard while an older Runner stopped its machine for no job (its start came late, as GitHub's messages do):
/// the job ran, so neither the slot nor the machine's end says "no job", and its completion is not reported so.
#[test]
fn a_job_an_older_runner_heard_while_it_stopped_the_machine_for_no_job_did_not_end_as_no_job() {
    for word in [Word::Started, Word::Completed] {
        for (phase, ended) in [
            (Phase::Released, Some("no-job")),
            (Phase::Stopping, None),
            (Phase::Killing, None),
        ] {
            let before = written(Old {
                phase,
                end: Some(End::NoJob),
                word,
                ran_job: false,
                unregistered: false,
                ended,
                end_reported: false,
                writer: Writer::Main,
            });
            let (after, _) = before.upgrade();
            let case = format!("{word:?} {phase:?} {ended:?}");
            assert_ne!(
                after.slot.expect("a slot").saved().end,
                Some(End::NoJob),
                "{case}"
            );
            assert_ne!(after.ended.map(|e| e.kind), Some("no-job".to_string()), "{case}");
        }
    }
}

/// A machine an older Runner saw end before its guest came up ran no job, whatever that Runner wrote of its
/// registration: GitHub is not asked, and a registration found gone is not taken for a job.
#[test]
fn a_machine_an_older_runner_saw_end_before_its_guest_came_up_is_not_asked_about() {
    for (end, kind) in [(End::StartFailed, "start-failed"), (End::Lost, "lost")] {
        for unregistered in [false, true] {
            let before = written(Old {
                phase: Phase::Released,
                end: Some(end),
                word: Word::None,
                ran_job: false,
                unregistered,
                ended: Some(kind),
                end_reported: false,
                writer: Writer::Main,
            });
            let (after, upgrade) = before.upgrade();
            let case = format!("{kind} unregistered: {unregistered}");
            assert_eq!(upgrade, Upgrade::Consistent, "{case}");
            assert!(!after.ask_github, "{case}");
            assert_eq!(after.unregistered, unregistered, "{case}");
            assert_eq!(after.clone().upgrade(), (after, Upgrade::Current), "{case}");
        }
    }
}

/// The machines that stand under a belief of an older Runner are not stopped when this one starts: their
/// registration's removal is asked for at once, and GitHub's answer decides.
#[tokio::test]
async fn a_standing_machine_an_older_runner_meant_to_stop_asks_github_first() {
    for (phase, end, unregistered) in [
        (Phase::Stopping, Some(End::NoJob), false),
        (Phase::Killing, Some(End::NoJob), false),
        (Phase::Stopping, Some(End::Stopped), false),
        (Phase::Killing, Some(End::Stopped), false),
        (Phase::Idle, None, true),
        (Phase::Stopping, Some(End::Stopped), true),
        (Phase::Starting, None, true),
    ] {
        let mut w = World::new(1);
        let runner = w.start().await;
        w.parts.vms.ready(job_name(&runner));
        pass(&w.m, 2);
        let mut record = w.parts.store.load().pop().expect("a record");
        let mut saved = record.slot.expect("a slot").saved();
        (saved.phase, saved.end) = (phase, end);
        record.slot = Some(saved.into());
        record.unregistered = unregistered;
        record.rules = 0;
        Store::save(w.parts.store.as_ref(), &record);
        let (stops, kills) = (w.parts.vms.stopped().len(), w.parts.vms.killed().len());
        w.restart();
        pass(&w.m, 5);
        let case = format!("{phase:?} {end:?} unregistered: {unregistered}");
        assert_eq!(
            (w.parts.vms.stopped().len(), w.parts.vms.killed().len()),
            (stops, kills),
            "{case}: stopped before GitHub was asked"
        );
        assert!(
            w.m.registrations_to_remove().iter().any(|r| r.runner == runner),
            "{case}: its removal is not asked for"
        );
        // GitHub answers with 204: no job, and only now does the machine stop.
        w.sweep();
        pass(&w.m, 2);
        assert_eq!(
            w.parts.vms.stopped().len(),
            stops + 1,
            "{case}: GitHub's 204 stops it"
        );
    }
}
