//! What the polls in flight offer GitHub, with several scale sets of one size that each ask at their own moment:
//! whatever order memory, slots and looks come in, the polls never offer more room than the Mac has, none goes on
//! offering a slot that is gone, and each scale set gets its turn at the room there is.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use cf_budget::{GIB, Size};
use cf_daemon::OFFER_STANDS;
use cf_daemon::testing::pass;
use cf_daemon::{Claim, SetKey, Slots, Ticket};
use proptest::prelude::*;
use support::world::{REPO, SIZE, World};

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

/// The listeners of one size, one per scale set, each with what its poll in flight offers: room for one more, or
/// none.
struct Listeners {
    offers: [bool; 4],
    rung: usize,
}

impl Listeners {
    fn new(w: &World) -> Listeners {
        let mut listeners = Listeners {
            offers: [false; 4],
            rung: w.nudges(),
        };
        listeners.all_ask(w);
        listeners
    }

    fn ask(&mut self, w: &World, i: usize) {
        self.offers[i] = w.m.offer_room(&set(i), SIZE) > 0;
    }

    fn all_ask(&mut self, w: &World) {
        for i in 0..self.offers.len() {
            self.ask(w, i);
        }
    }

    fn offering(&self) -> usize {
        self.offers.iter().filter(|&&offers| offers).count()
    }

    /// A bell that rang ends every poll: each listener asks again.
    fn hear(&mut self, w: &World) {
        let rung = w.nudges();
        if rung != self.rung {
            self.rung = rung;
            self.all_ask(w);
        }
    }
}

fn set(i: usize) -> SetKey {
    (REPO.to_string(), 100 + i64::try_from(i).expect("few"))
}

fn free(w: &World) -> usize {
    w.m.snapshot().room(SIZE).free.map_or(0, |free| free as usize)
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
                    let i = usize::from(i) % listeners.offers.len();
                    listeners.ask(&w, i);
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
        // Then they keep asking, the Mac left as it is: the polls never offer more room than it has, and while it
        // has some, each scale set gets its turn.
        // Every poll in flight has ended and asked again by then: none stands longer than `OFFER_STANDS`.
        listeners.all_ask(&w);
        listeners.hear(&w);
        let room = free(&w).min(listeners.offers.len());
        let mut offered = [false; 4];
        // A turn is two polls and the one that gives the room up: four scale sets have had theirs in sixteen.
        for _ in 0..16 {
            for (i, got) in offered.iter_mut().enumerate() {
                listeners.ask(&w, i);
                prop_assert!(listeners.offering() <= room, "{} polls offer room for {}", listeners.offering(), room);
                *got |= listeners.offers[i];
            }
            listeners.hear(&w);
            prop_assert!(listeners.offering() <= room, "{} polls offer room for {}", listeners.offering(), room);
        }
        prop_assert_eq!(offered, [room > 0; 4]);
    }
}

/// The Mac with room for one more machine: the other runs a job.
fn one_free() -> World {
    let w = World::new(2);
    assert!(matches!(w.m.claim(SIZE, None), Claim::Start(_)));
    assert_eq!(free(&w), 1);
    w
}

#[test]
fn scale_sets_share_the_room_their_polls_offer() {
    let w = one_free();
    assert_eq!(w.m.offer_room(&set(0), SIZE), 1);
    assert_eq!(
        w.m.offer_room(&set(1), SIZE),
        0,
        "the one free machine is offered by the first poll already"
    );
    assert_eq!(w.m.offer_room(&set(2), SIZE), 0);
    assert_eq!(
        w.m.offer_room(&set(0), SIZE),
        1,
        "a poll that asks again offers what its last one did"
    );
}

#[test]
fn room_a_poll_offered_is_offered_again_once_its_job_starts_or_the_room_goes() {
    let w = one_free();
    assert_eq!(w.m.offer_room(&set(0), SIZE), 1);
    // GitHub assigns the job: the machine is full, and no poll offers anything.
    let Claim::Start(job) = w.m.claim(SIZE, None) else {
        panic!("the offered room was there");
    };
    assert_eq!(w.m.offer_room(&set(0), SIZE), 0);
    assert_eq!(w.m.offer_room(&set(1), SIZE), 0);
    // It ends: the next poll to ask gets the room.
    w.m.release(job);
    assert_eq!(w.m.offer_room(&set(1), SIZE), 1);
    assert_eq!(w.m.offer_room(&set(0), SIZE), 0);
}

#[test]
fn a_size_offered_counts_against_every_other_size() {
    // Room for two machines of the size, or one twice as large.
    let w = World::new(2);
    assert_eq!(Size::G4.vm_bytes(), 2 * SIZE.vm_bytes());
    assert_eq!(w.m.offer_room(&set(0), Size::G4), 1);
    assert_eq!(
        w.m.offer_room(&set(1), SIZE),
        0,
        "the large machine offered takes the room of two"
    );
}

#[test]
fn a_scale_set_with_jobs_queued_goes_before_those_without() {
    let w = one_free();
    w.m.set_demand(BTreeMap::from([(set(0), false), (set(1), true), (set(2), false)]));
    assert_eq!(
        w.m.offer_room(&set(0), SIZE),
        1,
        "with no other poll offering, one without jobs offers too"
    );
    assert_eq!(
        w.m.offer_room(&set(1), SIZE),
        1,
        "one with jobs queued is not kept out by it"
    );
    assert_eq!(
        w.m.offer_room(&set(2), SIZE),
        0,
        "one without jobs is kept out by both"
    );
    assert_eq!(
        w.m.offer_room(&set(3), SIZE),
        0,
        "nor does one whose queue was not read get past them"
    );
}

#[test]
fn scale_sets_of_two_repositories_with_the_same_number_are_two() {
    // GitHub numbers each repository's scale sets from its own start: on one host, a busy repository's set and an idle
    // one's can both be 15.
    let w = one_free();
    let busy = ("codefusion-cc/busy".to_string(), 15);
    let idle = ("codefusion-cc/idle".to_string(), 15);
    w.m.set_demand(BTreeMap::from([(busy.clone(), true), (idle.clone(), false)]));
    assert_eq!(w.m.offer_room(&busy, SIZE), 1);
    assert_eq!(
        w.m.offer_room(&idle, SIZE),
        0,
        "the other repository's poll neither ends the offer nor gets the room"
    );
    assert_eq!(
        w.m.offer_room(&busy, SIZE),
        1,
        "the busy set's second poll offers the room again, so GitHub sees it twice"
    );
    w.m.withdraw_offer(&idle);
    assert_eq!(
        w.m.offer_room(&busy, SIZE),
        1,
        "nor does the other one's withdrawal take the room from it"
    );
}

#[test]
fn an_offer_github_never_fills_stands_no_longer_than_a_poll() {
    let w = one_free();
    assert_eq!(w.m.offer_room(&set(0), SIZE), 1);
    // Its listener asks no more: it stopped, or lost its session. The Mac's own clock may race meanwhile.
    pass(&w.m, 600);
    w.clock.advance(OFFER_STANDS - Duration::from_secs(1));
    assert_eq!(
        w.m.offer_room(&set(1), SIZE),
        0,
        "the first poll may still be in flight"
    );
    w.clock.advance(Duration::from_secs(1));
    assert_eq!(
        w.m.offer_room(&set(1), SIZE),
        1,
        "it has ended by now, and its room is offered again"
    );
}

/// A Google Compute Engine host of the CI pool: 15.6 GB and 8 CPUs, given to CI.
fn gcp() -> World {
    World::on(cf_budget::Settings::for_headless(
        cf_budget::HostFacts {
            total_bytes: 15_600 * 1024 * 1024,
            cores: 8,
        },
        8,
    ))
}

#[test]
fn a_machine_with_room_still_fills_every_slot_and_a_full_one_takes_nothing() {
    let w = gcp();
    let room = free_of(&w, Size::G4);
    assert_eq!(room, 3, "the pool's hosts run three cf-4g at once");
    // Every scale set asks: as many offer as there are slots, however many sets there are.
    let offered = (0..8).filter(|&i| w.m.offer_room(&set(i), Size::G4) > 0).count();
    assert_eq!(offered, 3);
    // Two of the jobs come: the third slot is still offered, by one poll.
    for _ in 0..2 {
        assert!(matches!(w.m.claim(Size::G4, None), Claim::Start(_)));
    }
    let offered = (0..8).filter(|&i| w.m.offer_room(&set(i), Size::G4) > 0).count();
    assert_eq!(offered, 1);
    // What else runs takes the memory: no poll offers anything, and no job is acquired.
    let floor = w.parts.settings.away.floor_bytes;
    w.m.with_host(|host| host.set(|r| r.available_bytes = floor + GIB));
    pass(&w.m, 1);
    let offered = (0..8).filter(|&i| w.m.offer_room(&set(i), Size::G4) > 0).count();
    assert_eq!(offered, 0);
    assert!(w.m.reserve(Size::G4).is_err(), "a full machine acquires no job");
}

fn free_of(w: &World, size: Size) -> usize {
    w.m.snapshot().room(size).free.map_or(0, |free| free as usize)
}

#[test]
fn jobs_queued_for_a_scale_set_end_the_polls_of_those_without() {
    let w = one_free();
    w.m.set_demand(BTreeMap::from([(set(0), false), (set(1), false)]));
    assert_eq!(w.m.offer_room(&set(0), SIZE), 1);
    let rung = w.nudges();
    w.m.set_demand(BTreeMap::from([(set(0), false), (set(1), false)]));
    assert_eq!(w.nudges(), rung, "nothing new is queued");
    w.m.set_demand(BTreeMap::from([(set(0), false), (set(1), true)]));
    assert_eq!(w.nudges(), rung + 1, "the polls ask again");
    assert_eq!(w.m.offer_room(&set(1), SIZE), 1);
}

#[test]
fn a_scale_set_that_stops_offering_gives_its_room_back_at_once() {
    let w = one_free();
    assert_eq!(w.m.offer_room(&set(0), SIZE), 1);
    assert_eq!(w.m.offer_room(&set(1), SIZE), 0);
    // Its job waited too long, or its listener went away: it offers nothing from now on.
    w.m.withdraw_offer(&set(0));
    assert_eq!(w.m.offer_room(&set(1), SIZE), 1, "not a minute later");
}
