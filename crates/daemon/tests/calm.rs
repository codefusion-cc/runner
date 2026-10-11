//! The calm time: a Mac opens after a minute in which nothing was wrong with it. However the clocks move on while
//! the Mac is read, that minute never starts again by itself; whatever is really wrong with the Mac starts it again.

mod support;

use cf_budget::{GIB, Host as _, HostError, Millis, Pressure, Refusal, Thermal};
use cf_daemon::testing::TestMachines;
use proptest::prelude::*;
use support::world::World;

/// The calm time of the test Mac's settings, the longest a reading may be old, and how long a swap-out counts.
const CALM: Millis = Millis::secs(60);
const STALE: Millis = Millis::secs(5);
const SWAP_QUIET: Millis = Millis::secs(30);

/// How long a look takes: until the reading gets its time, and from then until the read answers. A real Mac needs
/// well under a millisecond for each; these go far beyond that and stay far below [`STALE`].
fn reads(looks: usize) -> impl Strategy<Value = Vec<(u64, u64)>> {
    prop::collection::vec((0u64..=40, 0u64..=40), looks)
}

/// One second passes and the Mac is looked at, with a read that takes this long. Says when the look began and
/// when it was over, on the Mac's clock.
fn look(m: &TestMachines, (before, after): (u64, u64)) -> (Millis, Millis) {
    let began = m.with_host(|host| {
        host.pass(Millis::secs(1));
        host.read_takes(Millis(before), Millis(after));
        host.now().elapsed
    });
    m.tick();
    (began, m.with_host(|host| host.now().elapsed))
}

/// Something that is really wrong with the Mac, or with the look at it.
#[derive(Debug, Clone, Copy)]
enum Wrong {
    Pressure,
    /// macOS swaps pages out at every look, which counts for [`SWAP_QUIET`] after the last one.
    Swapping,
    UnderFloor,
    Hot,
    Battery,
    DiskFull,
    Unreadable,
    /// The read took longer than a reading may be old.
    ReadingTooOld,
    /// Nobody looked for longer than a reading may be old.
    NoLook,
}

impl Wrong {
    const ALL: [Wrong; 9] = [
        Wrong::Pressure,
        Wrong::Swapping,
        Wrong::UnderFloor,
        Wrong::Hot,
        Wrong::Battery,
        Wrong::DiskFull,
        Wrong::Unreadable,
        Wrong::ReadingTooOld,
        Wrong::NoLook,
    ];

    /// The budget's word for why the Mac is closed while this goes on.
    fn reason(self) -> &'static str {
        match self {
            Wrong::Pressure => "memory-pressure",
            Wrong::Swapping => "swapping",
            Wrong::UnderFloor => "under-floor",
            Wrong::Hot => "thermal",
            Wrong::Battery => "battery",
            Wrong::DiskFull => "disk",
            Wrong::Unreadable | Wrong::ReadingTooOld => "no-reading",
            Wrong::NoLook => "calming",
        }
    }

    fn set(self, m: &TestMachines, on: bool) {
        m.with_host(|host| match self {
            Wrong::Pressure => {
                host.set(|r| r.pressure = if on { Pressure::Warning } else { Pressure::Normal });
            }
            Wrong::UnderFloor => host.set(|r| r.available_bytes = if on { GIB } else { 64 * GIB }),
            Wrong::Hot => host.set(|r| r.thermal = if on { Thermal::Serious } else { Thermal::Nominal }),
            Wrong::Battery => host.set(|r| r.on_battery = on),
            Wrong::DiskFull => host.set(|r| r.disk_free_bytes = if on { GIB } else { 1024 * GIB }),
            Wrong::Unreadable => host.fail(on.then_some(HostError::Call {
                what: "host_statistics64",
                code: 5,
            })),
            Wrong::Swapping | Wrong::ReadingTooOld | Wrong::NoLook => {}
        });
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A Runner that has just started on a Mac with nothing wrong: the Mac is never without a reading, the time
    /// left of the calm minute only ever shrinks, and from the look a minute after the first one the Mac is open
    /// and stays so.
    #[test]
    fn time_that_passes_within_a_look_never_starts_the_calm_time_again(reads in reads(150)) {
        let mut w = World::new(2);
        w.restart();
        let mut left_before: Option<Millis> = None;
        let mut first = None;
        for (index, read) in reads.into_iter().enumerate() {
            let (began, over) = look(&w.m, read);
            let (_, first_over) = *first.get_or_insert((began, over));
            let snapshot = w.m.snapshot();
            match snapshot.budget.closed_by {
                None => left_before = Some(Millis::ZERO),
                Some(Refusal::Calming { left }) => {
                    prop_assert!(
                        left_before.is_none_or(|before| left <= before),
                        "look {index}: {left:?} of the calm time left after {left_before:?}"
                    );
                    left_before = Some(left);
                }
                Some(other) => prop_assert!(false, "look {index}: closed by {other:?} with nothing wrong"),
            }
            if began.since(first_over) >= CALM {
                prop_assert!(snapshot.open, "look {index}: closed ({}) a minute after the first look", snapshot.reason);
            }
        }
        prop_assert_eq!(left_before, Some(Millis::ZERO));
    }

    /// An open Mac closes for what is really wrong, with that as the reason, and opens a whole calm minute after
    /// the first look that found nothing wrong again: not sooner, and not later.
    #[test]
    fn what_is_really_wrong_starts_the_calm_time_again(
        reads in reads(150),
        wrong in prop::sample::select(Wrong::ALL.to_vec()),
        calm_looks in 0usize..40,
        wrong_looks in 1usize..5,
    ) {
        let w = World::new(2);
        let mut reads = reads.into_iter();
        for _ in 0..calm_looks {
            look(&w.m, reads.next().expect("a read"));
            prop_assert!(w.m.snapshot().open, "closed ({}) with nothing wrong", w.m.snapshot().reason);
        }
        // The last look with something wrong in it, and how long that counts after it.
        let mut last_wrong = None;
        match wrong {
            Wrong::NoLook => w.m.with_host(|host| host.pass(Millis(STALE.0 + 1))),
            Wrong::Swapping => {
                for _ in 0..wrong_looks {
                    w.m.with_host(|host| host.set(|r| r.swapouts += 1));
                    last_wrong = Some(look(&w.m, reads.next().expect("a read")));
                    prop_assert_eq!(w.m.snapshot().reason, wrong.reason());
                }
            }
            Wrong::ReadingTooOld => {
                for _ in 0..wrong_looks {
                    look(&w.m, (reads.next().expect("a read").0, STALE.0 + 1));
                    prop_assert_eq!(w.m.snapshot().reason, wrong.reason());
                }
            }
            _ => {
                wrong.set(&w.m, true);
                for _ in 0..wrong_looks {
                    look(&w.m, reads.next().expect("a read"));
                    prop_assert_eq!(w.m.snapshot().reason, wrong.reason());
                }
                wrong.set(&w.m, false);
            }
        }
        // The first look that finds nothing wrong: from somewhere within it the calm minute counts. After a
        // swap-out that is the first look the quiet time is over at, which may be either of two.
        let mut first_began = None;
        let first_over = loop {
            let (began, over) = look(&w.m, reads.next().expect("a read"));
            if let Some((wrong_began, wrong_over)) = last_wrong {
                if over.since(wrong_began) < SWAP_QUIET {
                    prop_assert_eq!(w.m.snapshot().reason, wrong.reason());
                    continue;
                }
                first_began.get_or_insert(began);
                if began.since(wrong_over) < SWAP_QUIET {
                    continue;
                }
            }
            first_began.get_or_insert(began);
            break over;
        };
        let first_began = first_began.expect("a first look");
        prop_assert_eq!(w.m.snapshot().reason, "calming", "{:?}", wrong);
        let mut opened = false;
        for read in reads {
            let (began, over) = look(&w.m, read);
            let snapshot = w.m.snapshot();
            if over.since(first_began) < CALM {
                prop_assert_eq!(snapshot.reason.as_str(), "calming", "{:?}: open before the minute was over", wrong);
            }
            if began.since(first_over) >= CALM {
                prop_assert!(snapshot.open, "{:?}: closed ({}) after the minute", wrong, snapshot.reason);
                opened = true;
            }
        }
        prop_assert!(opened);
    }
}
