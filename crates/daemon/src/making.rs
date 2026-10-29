//! Room made for a larger size. A Mac that runs a smaller job, with more of them assigned for the slot beside it,
//! may never be empty enough for a larger one: every slot that frees is taken by the next small job, the larger
//! job waits in line, and GitHub takes it back after five minutes and hands it out again, for hours.
//!
//! So once a job of a size larger than every machine that runs waits in line for the room those machines hold, no
//! other size starts until it does: the budget holds them back ([`cf_budget::Budget::make_room`]) and puts it first
//! in line. Its scale set keeps offering a place in line while GitHub has taken the job back, so that GitHub hands
//! it here again. A size whose polls have found no room for long is offered that place before any of its jobs is
//! assigned, since none would be otherwise. Room is made for one size at a time, for half an hour at most; a size
//! the empty Mac cannot run is never offered a place.
//!
//! A headless host is one of many that serve the same jobs. One that offers a place while it is full wins the job
//! from those with room, holds back its other sizes for it, and loses it when GitHub takes it back five minutes
//! later. So a headless host makes room, and offers a place, only for the size the console names it
//! ([`Making::name`]): the console sees every host and names one, for a size none has room for.

use cf_budget::{Budget, Millis, Now, Refusal, Settings, Size, Status};

/// How long room is made for one size at most. Then its job waits in line as any job does, until GitHub takes it
/// back, and no room is made, and no place offered, for as long again.
pub const MAKE_ROOM_LIMIT: Millis = Millis::mins(30);

/// How long the polls of a size must have found no room before its scale set offers a place in line.
pub const STARVED_AFTER: Millis = Millis::mins(10);

/// How long room is made for a job GitHub took back before it is given up: GitHub hands the job out again at once
/// to a scale set that offers a place, so one that does not come back was cancelled or went elsewhere.
pub const TAKEN_BACK_WAIT: Millis = Millis::mins(2);

/// How long an empty Mac on which the size still lacks memory makes room before it gives up: what else runs on the
/// Mac holds it, and no machine of the Runner's that ends can free it.
pub const NO_ROOM_WAIT: Millis = Millis::mins(2);

/// Room being made for one size.
#[derive(Clone, Copy, Debug)]
struct Room {
    size: Size,
    /// When it began.
    since: Millis,
    /// Since when no job of the size waits in line: GitHub took it back.
    alone: Option<Millis>,
    /// Since when the Mac holds none of its own machines and the size still lacks memory.
    empty: Option<Millis>,
}

/// For which size room is made, and what decides when it is.
#[derive(Debug, Default)]
pub struct Making {
    room: Option<Room>,
    /// Until when no room is made after room that could not be made was given up.
    resting: Option<Millis>,
    /// Since when each size's polls have found no room.
    starved: [Option<Millis>; 4],
    /// The size the console named this host to make room for: on a headless host, the only one it makes room for.
    named: Option<Size>,
}

/// What a poll of one size offers GitHub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offered {
    /// This many machines of the size fit now.
    Free(u32),
    /// None fits now, but a job of the size gets a place in line ([`Making::place`]).
    Place,
    /// Nothing.
    Nothing,
}

impl Making {
    fn size(&self) -> Option<Size> {
        self.room.map(|room| room.size)
    }

    /// The size the console names this host to make room for, or none. Says whether that changed.
    pub fn name(&mut self, size: Option<Size>) -> bool {
        let changed = std::mem::replace(&mut self.named, size) != size;
        if changed {
            match size {
                Some(size) => tracing::info!(
                    size = size.label(),
                    "the console names this host to make room for a size no host has room for"
                ),
                None => tracing::info!("the console names this host to make room for no size"),
            }
        }
        changed
    }

    /// Whether room may be made, and a place offered, for `size` here: on a Mac, which decides alone, for every
    /// size; on a headless host only for the one the console names.
    fn may_make(&self, settings: &Settings, size: Size) -> bool {
        !settings.headless || self.named == Some(size)
    }

    /// Before every decision of the budget's: makes room for the first job in line that waits for room the Mac's
    /// own machines hold, all of them smaller; gives up room that has been made for too long, for a job GitHub took
    /// back and did not hand here again, or on an empty Mac that has no room for the size anyway; and tells the
    /// budget what room is made for.
    pub fn check(&mut self, budget: &mut Budget, now: Now) {
        let rests = self.rests(now);
        let named = self.named;
        let may_make = |settings: &Settings, size: Size| !settings.headless || named == Some(size);
        match &mut self.room {
            Some(room) => {
                let size = room.size;
                let status = budget.status(now);
                // What keeps its job waiting, or one more machine of the size from starting while GitHub has the
                // job back.
                let waiter = status.waiting.iter().find(|waiter| waiter.size == size);
                let why = waiter.map_or_else(|| budget.fits(now, size).err(), |waiter| waiter.why);
                let short = matches!(why, Some(Refusal::Memory { .. }));
                room.alone = match waiter {
                    Some(_) => None,
                    None => room.alone.or(Some(now.elapsed)),
                };
                room.empty = if short && status.slots.is_empty() {
                    room.empty.or(Some(now.elapsed))
                } else {
                    None
                };
                let past = |since: Option<Millis>, wait: Millis| {
                    since.is_some_and(|at| now.elapsed.since(at) >= wait)
                };
                let end = if budget.settings().can_run(size).is_err() {
                    Some((false, "the settings no longer run this size"))
                } else if !may_make(budget.settings(), size) {
                    Some((false, "the console names another host to make room for this size"))
                } else if now.elapsed.since(room.since) >= MAKE_ROOM_LIMIT {
                    Some((true, "room was made for half an hour and is still not there"))
                } else if past(room.empty, NO_ROOM_WAIT) {
                    Some((
                        true,
                        "the host is empty and still has no room: what else runs on it holds the memory",
                    ))
                } else if past(room.alone, TAKEN_BACK_WAIT) {
                    Some((false, "GitHub took the job back and did not hand it here again"))
                } else {
                    None
                };
                if let Some((rest, why)) = end {
                    tracing::warn!(
                        size = size.label(),
                        why,
                        "no more room is made for this size: the others start again"
                    );
                    self.room = None;
                    if rest {
                        self.resting = Some(now.elapsed.plus(MAKE_ROOM_LIMIT));
                    }
                }
            }
            // Room is made only beside machines of the Mac's own ([`worth`]).
            None if budget.waiting() > 0 && budget.slots() > 0 && !rests => {
                let status = budget.status(now);
                // The first in line waits for what keeps it out; those behind it, for it. Only room it waits for
                // is room to make: not a hold, a trouble or a Mac that calms down.
                let mut line = status.waiting.iter().filter(|waiter| !waiter.never).peekable();
                let first = match line.peek() {
                    Some(first) if held_back(first.why) => line.find(|waiter| {
                        may_make(budget.settings(), waiter.size)
                            && worth(budget.settings(), &status, waiter.size)
                    }),
                    _ => None,
                };
                if let Some(waiter) = first {
                    tracing::info!(
                        size = waiter.size.label(),
                        limit_min = MAKE_ROOM_LIMIT.0 / 60_000,
                        "a job of this size waits for room the smaller ones keep taking: none of them starts until it does"
                    );
                    self.room = Some(Room {
                        size: waiter.size,
                        since: now.elapsed,
                        alone: None,
                        empty: None,
                    });
                }
            }
            None => {}
        }
        budget.make_room(self.size());
    }

    /// A machine of `size` got its slot: the room is made, and the budget lets the other sizes start again.
    pub fn started(&mut self, budget: &mut Budget, size: Size) {
        if self.size() == Some(size) {
            tracing::info!(
                size = size.label(),
                "the room is made: a machine of this size starts, and the others may again"
            );
            self.room = None;
            budget.make_room(None);
        }
    }

    /// What a poll of `size` offers GitHub: as many machines as fit, or a place in line.
    pub fn offer(&mut self, budget: &Budget, now: Now, size: Size) -> Offered {
        let at = size.index();
        let free = budget.free(now).of(size);
        if free > 0 {
            self.starved[at] = None;
            return Offered::Free(free);
        }
        self.starved[at].get_or_insert(now.elapsed);
        match budget.fits(now, size) {
            Err(why) if self.place(budget, now, size, why) => Offered::Place,
            _ => Offered::Nothing,
        }
    }

    /// Whether a poll of `size`, of which no machine fits for `why`, offers a place in line: the size room is made
    /// for, while GitHub has taken its job back; or a size whose polls have found no room for [`STARVED_AFTER`],
    /// for which room would be made ([`Making::check`]) once a job of it waits.
    pub fn place(&self, budget: &Budget, now: Now, size: Size, why: Refusal) -> bool {
        if !self.may_make(budget.settings(), size) {
            return false;
        }
        if let Some(room) = self.room {
            return room.size == size && room.alone.is_some();
        }
        // The console names a size once none of the hosts had room for it long enough: this host need not have
        // found none for as long itself.
        let starved = self.named == Some(size)
            || self.starved[size.index()].is_some_and(|since| now.elapsed.since(since) >= STARVED_AFTER);
        if !starved || self.rests(now) || !held_back(Some(why)) || budget.slots() == 0 {
            return false;
        }
        // A job of the size that waits has room made for it already ([`Making::check`]), unless it is one to make
        // none for, which this is not either.
        worth(budget.settings(), &budget.status(now), size)
    }

    fn rests(&self, now: Now) -> bool {
        self.resting.is_some_and(|until| now.elapsed < until)
    }
}

/// Whether what keeps a machine out is what the Mac's own machines, or the jobs in line, hold: room that making room
/// frees. Not a hold, a trouble, the person or a Mac that calms down.
fn held_back(why: Option<Refusal>) -> bool {
    matches!(
        why,
        Some(
            Refusal::Memory { .. }
                | Refusal::Full { .. }
                | Refusal::Cpus { .. }
                | Refusal::Machines { .. }
                | Refusal::AssignedFirst { .. }
        )
    )
}

/// Whether making room for `size` is worth it: the Mac holds machines of its own, every one smaller, so that none
/// that ends leaves its room to a job of the size by itself, and the Mac would run one without them.
fn worth(settings: &Settings, status: &Status, size: Size) -> bool {
    !status.slots.is_empty()
        && status.slots.iter().all(|held| held.size < size)
        && theirs(settings, status, size)
}

/// Whether the memory one more machine of this size lacks is held, or still to be taken, by the Mac's own
/// machines: a Mac that would carry it without them is full, not short.
pub fn theirs(settings: &Settings, status: &Status, size: Size) -> bool {
    let without_machines = status
        .reading
        .map(|reading| reading.available_bytes.saturating_add(status.resident_bytes));
    without_machines
        .is_some_and(|available| cf_budget::admit(settings, &status.limits, available, &[], size).is_ok())
}
