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
    /// Whether each size's polls were last offered a place in line, so that the log says it once.
    placed: [bool; 4],
}

impl Making {
    /// The size room is made for.
    pub fn size(&self) -> Option<Size> {
        self.room.map(|room| room.size)
    }

    /// Before every decision of the budget's: makes room for the first job in line that waits for room the Mac's
    /// own machines hold, all of them smaller; gives up room that has been made for too long, for a job GitHub took
    /// back and did not hand here again, or on an empty Mac that has no room for the size anyway; and tells the
    /// budget what room is made for.
    pub fn check(&mut self, budget: &mut Budget, now: Now) {
        budget.make_room(self.size());
        let rests = self.rests(now);
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
                let end = if past(Some(room.since), MAKE_ROOM_LIMIT) {
                    Some((true, "room was made for half an hour and is still not there"))
                } else if past(room.empty, NO_ROOM_WAIT) {
                    Some((
                        true,
                        "the Mac is empty and still has no room: what else runs on it holds the memory",
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
            None if budget.waiting() > 0 && !rests => {
                let status = budget.status(now);
                let first = status.waiting.iter().find(|waiter| {
                    !waiter.never && held_back(waiter.why) && worth(budget.settings(), &status, waiter.size)
                });
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

    /// A machine of `size` got its slot: the room is made.
    pub fn started(&mut self, size: Size) {
        if self.size() == Some(size) {
            tracing::info!(
                size = size.label(),
                "the room is made: a machine of this size starts, and the others may again"
            );
            self.room = None;
        }
    }

    /// How many machines of `size` a poll offers GitHub: as many as fit, or a place in line.
    pub fn offer(&mut self, budget: &Budget, now: Now, size: Size) -> u32 {
        let at = size.index();
        let free = budget.free(now).of(size);
        if free > 0 {
            self.starved[at] = None;
            self.placed[at] = false;
            return free;
        }
        self.starved[at].get_or_insert(now.elapsed);
        let place = self.place(budget, now, size);
        if place && !self.placed[at] {
            tracing::info!(
                size = size.label(),
                "no machine of this size fits now, but a job of it gets a place in line: the smaller ones wait for it"
            );
        }
        self.placed[at] = place;
        u32::from(place)
    }

    /// Whether a poll of `size` offers a place in line although no machine of it fits now: the size room is made
    /// for, while GitHub has taken its job back; or a size whose polls have found no room for
    /// [`STARVED_AFTER`], for which room would be made ([`Making::check`]) once a job of it waits.
    pub fn place(&self, budget: &Budget, now: Now, size: Size) -> bool {
        if let Some(room) = self.room {
            return room.size == size && room.alone.is_some();
        }
        let starved =
            self.starved[size.index()].is_some_and(|since| now.elapsed.since(since) >= STARVED_AFTER);
        if !starved || self.rests(now) || !held_back(budget.fits(now, size).err()) {
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
