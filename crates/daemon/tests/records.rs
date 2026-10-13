//! Records an older Runner left, read by this one ([`Record::upgrade`]). Every combination of what the Runner of
//! main (`d8f202c`) and the builds of this branch before these rules could have written is read: each comes out with
//! facts that agree, none is stopped or forgotten on a belief, and reading it again changes nothing.

mod support;

use cf_daemon::names::job_name;
use cf_daemon::testing::pass;
use cf_daemon::{Disagrees, RULES, Record, Store, Upgrade};
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
    /// Rules 1: `unregistered` and an end without a job only on GitHub's answer, and an idle slot with that end
    /// asking for its removal.
    Rules1 { ask_github: bool },
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
        Writer::Rules1 { ask_github } => {
            record["ask_github"] = json!(ask_github);
            record["rules"] = json!(1);
        }
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
        Writer::Rules1 { ask_github: false },
        Writer::Rules1 { ask_github: true },
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

/// Whether the record's VM came up, by what an older Runner could have written: a job, or a phase that waits for or
/// runs one, or one past the boot that no failed start left.
fn came_up(old: &Old) -> bool {
    old.ran_job
        || !matches!(old.word, Word::None)
        || matches!(old.phase, Phase::Idle | Phase::Running | Phase::Finishing)
        || (matches!(old.phase, Phase::Stopping | Phase::Killing | Phase::Released)
            && !matches!(old.end, Some(End::StartFailed | End::Lost)))
}

/// What these rules forbid to follow from a record an older Runner wrote.
fn acted_on_belief(old: &Old, before: &Record, after: &Record) -> Option<&'static str> {
    let saved = after.slot.expect("a slot").saved();
    if after.rules != RULES || !after.agrees() {
        return Some("left with facts that disagree");
    }
    if before.github != after.github {
        return Some("GitHub's word changed");
    }
    if saved.up != came_up(old) {
        return Some("whether the VM came up read wrong");
    }
    let trusted = matches!(old.writer, Writer::Rules1 { .. });
    let gone = before.ended.is_some();
    if trusted && after.unregistered != before.unregistered {
        return Some("GitHub's answer of rules 1 dropped");
    }
    if after.github.is_none() && !trusted {
        if !gone && after.unregistered {
            return Some("an older Runner's `unregistered` of a standing machine taken as GitHub's 204");
        }
        if gone && saved.up && (after.unregistered || after.end_reported) {
            return Some("a machine gone with no word of a job is not asked about");
        }
        if gone && !saved.up && after.unregistered != before.unregistered {
            return Some("a machine whose guest never came up is asked about");
        }
    }
    if after.github.is_none() && !trusted && after.finished() && saved.up {
        return Some("forgotten with no word of a job");
    }
    let standing = !gone;
    let stopped_for_no_job = matches!(saved.phase, Phase::Stopping | Phase::Killing)
        && matches!(saved.end, Some(End::NoJob | End::Stopped));
    let stopped_before = matches!(old.phase, Phase::Stopping | Phase::Killing) || old.end.is_some();
    let answered = trusted && (stopped_before || before.unregistered);
    if standing && after.github.is_none() && saved.up && stopped_for_no_job && !answered {
        return Some("a standing machine stopped for no job before GitHub was asked");
    }
    let removed = trusted && before.unregistered;
    if standing
        && old.phase == Phase::Idle
        && old.end == Some(End::NoJob)
        && after.github.is_none()
        && !removed
        && saved.phase != Phase::Asking
    {
        return Some("an idle machine that asked does not ask");
    }
    if saved.phase == Phase::Running && after.github.as_ref().is_some_and(|g| g.result.is_some()) {
        return Some("runs a job GitHub said is over");
    }
    None
}

#[test]
fn every_record_an_older_runner_could_write_goes_on_with_facts_that_agree() {
    let mut asking = 0;
    for old in all() {
        let before = written(old);
        let (after, upgrade) = before
            .clone()
            .upgrade()
            .unwrap_or_else(|e| panic!("{e}: {old:?}"));
        assert_eq!(upgrade, Upgrade::Upgraded, "{old:?}");
        if let Some(wrong) = acted_on_belief(&old, &before, &after) {
            panic!("{wrong}: {old:?}\nread as {after:?}");
        }
        asking += usize::from(after.slot.expect("a slot").saved().phase == Phase::Asking);
        // Read once: what it was made into is a record of these rules, and reading it again changes nothing.
        assert_eq!(after.clone().upgrade(), Ok((after, Upgrade::Current)), "{old:?}");
    }
    assert!(asking > 0, "no older record asks for its removal");
}

/// A record written under these rules whose facts disagree is the Runner's own mistake: an error, never read as an
/// older Runner's and changed.
#[test]
fn a_current_record_whose_facts_disagree_is_an_error() {
    let old = Old {
        phase: Phase::Running,
        end: None,
        word: Word::Completed,
        ran_job: true,
        unregistered: false,
        ended: None,
        end_reported: false,
        writer: Writer::Main,
    };
    let mut record = written(old);
    record.rules = RULES;
    let mut saved = record.slot.expect("a slot").saved();
    saved.up = true;
    record.slot = Some(saved.into());
    assert!(!record.agrees(), "a running job GitHub said is over");
    assert_eq!(record.clone().upgrade(), Err(Disagrees(Box::new(record))));
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
    let rows: [(&str, Old, Phase, bool); 17] = [
        (
            "`start`: a machine boots",
            main(Phase::Starting, None, Unheard, false, None, false),
            Phase::Starting,
            false,
        ),
        (
            "a 404 under a machine whose boot was not yet written",
            main(Phase::Starting, None, Unheard, true, None, false),
            Phase::Starting,
            false,
        ),
        (
            "`Events::job` died between its two writes, booting",
            main(Phase::Starting, None, Started, false, None, false),
            Phase::Running,
            false,
        ),
        (
            "`Events::job` died between its two writes, idle",
            main(Phase::Idle, None, Started, false, None, false),
            Phase::Running,
            false,
        ),
        (
            "`Events::job` died between the completion and its step",
            main(Phase::Running, None, Completed, false, None, false),
            Phase::Finishing,
            false,
        ),
        (
            "`retire_surplus`'s removal met 404 under a job whose start came late: the job is over",
            main(Phase::Running, None, Started, true, None, false),
            Phase::Finishing,
            false,
        ),
        (
            "the idle limit stopped it for no job",
            main(Phase::Stopping, Some(End::NoJob), Unheard, false, None, false),
            Phase::Asking,
            false,
        ),
        (
            "the idle limit's stop, then the kill",
            main(Phase::Killing, Some(End::NoJob), Unheard, false, None, false),
            Phase::Asking,
            false,
        ),
        (
            "`retire_surplus` stopped it after a 204 or a 404",
            main(Phase::Stopping, Some(End::Stopped), Unheard, true, None, false),
            Phase::Asking,
            false,
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
            Phase::Stopping,
            false,
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
            Phase::Released,
            false,
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
            Phase::Released,
            false,
        ),
        (
            "`unregistered` after the sweep's 404",
            main(Phase::Released, Some(End::Ok), Unheard, true, Some("ok"), false),
            Phase::Released,
            false,
        ),
        (
            "`unregistered` and `settle`, died before the removal",
            main(Phase::Released, Some(End::Ok), Unheard, true, Some("ok"), true),
            Phase::Released,
            false,
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
            Phase::Released,
            false,
        ),
        (
            "a job heard, both reported, died before the removal",
            main(Phase::Released, Some(End::Ok), Completed, true, Some("ok"), true),
            Phase::Released,
            true,
        ),
        (
            "a start that failed, its registration removed",
            main(
                Phase::Released,
                Some(End::StartFailed),
                Unheard,
                true,
                Some("start-failed"),
                true,
            ),
            Phase::Released,
            true,
        ),
    ];
    for (row, old, phase, finished) in rows {
        let before = written(old);
        let (after, upgrade) = before.clone().upgrade().expect("an older record is read");
        assert_eq!(upgrade, Upgrade::Upgraded, "{row}");
        assert_eq!(
            (after.slot.expect("a slot").saved().phase, after.finished()),
            (phase, finished),
            "{row}"
        );
        assert_eq!(acted_on_belief(&old, &before, &after), None, "{row}");
    }
}

/// A build of this branch under rules 1 wrote an idle slot with an end of "no job" for one asking for its
/// registration's removal: it asks still, its end not yet decided.
#[test]
fn an_idle_slot_rules_1_wrote_as_asking_asks() {
    let before = written(Old {
        phase: Phase::Idle,
        end: Some(End::NoJob),
        word: Word::None,
        ran_job: false,
        unregistered: false,
        ended: None,
        end_reported: false,
        writer: Writer::Rules1 { ask_github: false },
    });
    let (after, _) = before.upgrade().expect("read");
    let saved = after.slot.expect("a slot").saved();
    assert_eq!((saved.phase, saved.end, saved.up), (Phase::Asking, None, true));
    assert!(after.unheard());
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
            let (after, _) = before.upgrade().expect("read");
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
            let (after, _) = before.upgrade().expect("read");
            let case = format!("{kind} unregistered: {unregistered}");
            assert!(!after.unheard(), "{case}");
            assert_eq!(after.unregistered, unregistered, "{case}");
            assert_eq!(after.clone().upgrade(), Ok((after, Upgrade::Current)), "{case}");
        }
    }
}

/// The machines that stand under a belief of an older Runner are not stopped when this one starts: their
/// registration's removal is asked for, and GitHub's answer decides.
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
        // A machine written as booting asks once it is idle for the idle limit: main's `unregistered` of it may be
        // a 404 of a job that ran, so it is not taken for GitHub's word.
        pass(&w.m, if phase == Phase::Starting { 10 * 60 + 5 } else { 5 });
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
