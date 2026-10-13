//! What the polls in flight offer GitHub, with several scale sets of one size that each ask at their own moment:
//! whatever order memory, slots and looks come in, no poll goes on offering a slot that is gone, or none while
//! there is one, once the Mac was looked at for a few seconds.

mod support;

use cf_budget::GIB;
use cf_daemon::testing::pass;
use cf_daemon::{Claim, Slots, Ticket};
use proptest::prelude::*;
use support::world::{SIZE, World};

/// One thing that happens. A number picks among what is there at that moment.
#[derive(Debug, Clone)]
enum Op {
    /// A listener's poll ends by itself and it asks again.
    Asks(u8),
    /// What else runs on the Mac leaves room for a machine, or for none.
    Memory(bool),
    /// A scaler promises GitHub a place.
    Promise,
    /// A promise or a machine's slot is given back.
    GiveBack(u8),
    /// GitHub assigns a job.
    Assign,
    /// A job that waits in line is cancelled.
    Withdraw(u8),
    /// The Mac is told to take no jobs, or to take them again.
    Pause(bool),
    /// Seconds pass, with a look each.
    Seconds(u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => any::<u8>().prop_map(Op::Asks),
        4 => any::<bool>().prop_map(Op::Memory),
        2 => Just(Op::Promise),
        2 => any::<u8>().prop_map(Op::GiveBack),
        2 => Just(Op::Assign),
        1 => any::<u8>().prop_map(Op::Withdraw),
        1 => any::<bool>().prop_map(Op::Pause),
        6 => (1u8..4).prop_map(Op::Seconds),
    ]
}

/// The listeners of one size, each with what its poll in flight offers: room for one more, or none.
struct Listeners {
    offers: [bool; 4],
    rung: usize,
}

impl Listeners {
    fn new(w: &World) -> Listeners {
        Listeners {
            offers: [w.m.open_slots(SIZE) > 0; 4],
            rung: w.nudges(),
        }
    }

    /// A bell that rang ends every poll: each listener asks again.
    fn hear(&mut self, w: &World) {
        let rung = w.nudges();
        if rung != self.rung {
            self.rung = rung;
            self.offers = [w.m.open_slots(SIZE) > 0; 4];
        }
    }
}

fn take<T>(from: &mut Vec<T>, i: u8) -> Option<T> {
    (!from.is_empty()).then(|| from.remove(usize::from(i) % from.len()))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn no_poll_goes_on_offering_what_the_mac_no_longer_has(ops in prop::collection::vec(op(), 0..40)) {
        let w = World::new(2);
        let mut listeners = Listeners::new(&w);
        let mut held: Vec<Ticket> = Vec::new();
        let mut waiting: Vec<Ticket> = Vec::new();
        for op in ops {
            match op {
                Op::Asks(i) => {
                    listeners.offers[usize::from(i) % listeners.offers.len()] = w.m.open_slots(SIZE) > 0;
                }
                Op::Memory(room) => {
                    let bytes = if room { 64 * GIB } else { w.parts.settings.away.floor_bytes + GIB };
                    w.m.with_host(|host| host.set(|r| r.available_bytes = bytes));
                }
                Op::Promise => held.extend(w.m.reserve(SIZE)),
                Op::GiveBack(i) => {
                    if let Some(ticket) = take(&mut held, i) {
                        w.m.release(ticket);
                    }
                }
                Op::Assign => match w.m.claim(SIZE, None) {
                    Claim::Start(ticket) => held.push(ticket),
                    Claim::Waits { ticket, .. } => waiting.push(ticket),
                    Claim::Never(why) => panic!("the size is one the Mac runs: {why}"),
                },
                Op::Withdraw(i) => {
                    if let Some(ticket) = take(&mut waiting, i) {
                        w.m.withdraw(ticket);
                    }
                }
                Op::Pause(paused) => w.m.pause(paused),
                Op::Seconds(seconds) => {
                    for _ in 0..seconds {
                        pass(&w.m, 1);
                        listeners.hear(&w);
                    }
                }
            }
            listeners.hear(&w);
        }
        // A look tells the listeners every five seconds at most: after six, each has heard what is true.
        for _ in 0..6 {
            pass(&w.m, 1);
            listeners.hear(&w);
        }
        let room = w.m.snapshot().room(SIZE).free.is_some_and(|free| free > 0);
        prop_assert_eq!(listeners.offers, [room; 4]);
    }
}
