# What the Runner knows about a machine and its job

Every Actions runner the Runner registers has one record (`crates/daemon/src/records.rs`). The record stores facts,
and everything else is derived from them: whether GitHub must be asked, which registrations the sweep removes, what
the console hears. Three sources bring facts, and each can be stale or lost:

- **GitHub's session messages** (`JobStarted`, `JobCompleted`). They are lost with a session that ends or is
  forgotten, and arrive up to a minute late.
- **GitHub's answers**: to the Runner's own `DELETE` of a registration (the sweep), and the REST view at a session's
  opening (`reconcile`), of which only `busy: true` is used. Every answer goes through one call,
  `Machines::answered(runner, Answer)`.
- **The vm-host**: whether the guest came up. A guest that never came up ran no job.

The Runner's own record keeps only what was written; a crash loses whatever was in memory.

## The facts

| Fact | Stored as | Set by |
|---|---|---|
| The VM came up | `slot.up` | `VmReady`, a word of a job, the vm-host's boot time at restore |
| A job ran | `slot.ran_job`, `github: Some` | GitHub's word, or an answer that implies one (`github.inferred: true`) |
| GitHub said the job is over before the VM was up | `slot.completed` | `JobCompleted` heard while it booted |
| The registration is gone | `unregistered` | GitHub's 204 or 404 |
| The machine is gone | `ended` | the vm-host |
| Reported | `start_reported`, `end_reported` | the outbox took the report |

Whether GitHub must be asked is derived: the VM came up, nobody heard of a job, and the registration is still there
(`Record::unheard`). Every write agrees with itself (`Record::agrees`): a slot that waits for, asks about, runs or
finishes a job came up; one that asks heard of no job and still has its registration; an end without a job stands
only beside GitHub's 204 and no job; a job GitHub named ran, on a slot that no longer boots or waits; none runs a job
GitHub said is over.

## GitHub's answer to a removal

| Answer | HTTP | What it proves | The lifecycle hears |
|---|---|---|---|
| `Removed` | 204 to the Runner's `DELETE` | no job ran | `Unregistered`: the machine stops as `no-job` |
| `Gone` | 404, under a VM that came up or a job heard | its one job is over: a one-job Actions runner removes its own registration | `JobCompleted`, the job inferred if none was heard |
| `Gone` | 404, under a VM that never came up | nothing ran | `Unregistered` |
| `Refused` | 422 | a job holds the registration | `JobStarted`, inferred if none was heard |
| `Busy` | REST `busy: true` at a session's opening | the same | `JobStarted` |

A 5xx, a timeout or a lost answer proves nothing: the removal is tried again at the next sweep. Only a 204 says no
job ran.

## Asking for a removal

`Asking` is a phase of the lifecycle, so the removals come from one place: the sweep (`Service::sweep`) removes the
registration of every slot that asks, and of every machine that ended while the registration stands (once the wait
for GitHub's word, `RESULT_WAIT`, is over for one nobody heard of a job on). Two ways lead into `Asking`:

- **The idle limit.** A machine idle past `limits.idle` is not stopped on that belief, since its start may have gone
  with a session: it asks. With no answer for `limits.job` (GitHub away, no client for the repository), it is stopped
  as `timeout`: a job whose start went unheard began before the ask, so it is over its own time by then.
- **Surplus.** The scaler retires idle Actions runners past `IDLE_GRACE` (`Hosts::retire`, the event `Retire`). Those
  of the scale set already asking count toward the surplus. Only an idle machine asks; one still booting is retired
  at a later pass, once up, and one running is never retired.

A start heard while asking runs the job; a stop or an eviction ends the ask.

## The registration

`registering` (an intent written before GitHub is asked), then registered (the record has `runner_id`), then
`unregistered` (written in the step that heard GitHub's 204 or 404, together with what the lifecycle does with it).
The record is deleted once it is unregistered, its end is reported and its machine is gone.

## The rules

1. **A count from before an event is never used after it.** GitHub's counts arrive with messages. The REST look
   at a session's opening (`reconcile`) runs before the session is opened, so the opening counts postdate every
   answer it acted on. The Runner's own count only ever lags behind GitHub's into fewer machines: a machine gone
   with its job unheard counts until it is settled (`lost_with_job`). A gone machine that came up, with no word of a
   job, counts until its registration's removal is answered, `RESULT_WAIT` plus a sweep: about two and a half
   minutes in which GitHub's counts may be short of one machine. Counting it less could start a machine for a job
   that already ran, and the rule is never more machines than GitHub assigned.
2. **Facts that change together are one write.** GitHub's word of a job and the lifecycle step it causes are one
   write (`Machines::hear`), and so are `unregistered` and the step of GitHub's answer.
3. **A word of a job is never dropped for the phase it meets.** The lifecycle answers every phase and event. A start
   or an end heard while the machine boots means it was up; heard before there is a VM, it is kept and applied once
   one is up; heard after the slot is over, it is kept as the fact that a job ran, and an end of `no-job` becomes
   `stopped`.
4. **An end is reported exactly once for every reported start, whichever path noticed it first**, under the ids
   `<runner>/started` and `<runner>/completed`. Every start and end goes through `Machines`.
5. **The fake takes back an assignment not taken within five minutes**, as GitHub does (`docs/github-protocol.md`,
   probe J2): a `JobCompleted` `canceled` with no Actions runner, and the job is queued again. **Its removals fail
   until GitHub recovers**, not for a count of tries the client's retries use up in milliseconds.

## Order of writes and acts, and what may repeat

| Step | Written first | Then done | After a crash between them |
|---|---|---|---|
| Register | intent record (`registering`) | `generatejitconfig` | The sweep looks for the name and removes it |
| Start a machine | the record with its slot | the vm-host starts | No machine: the record ends as `daemon-restart` |
| Hear a start | `github`, `start_reported` and the lifecycle, one write | the report | Reported again under the same id |
| Ask for a removal | the slot in `Asking` | `DELETE` at the next sweep | Asked again after the restart |
| Hear a removal | `unregistered` and the lifecycle's step, one write | the machine stops | The restored record replays it |
| Report a start | the report (outbox, id `<runner>/started`) | `start_reported` | Reported again under the same id; the outbox drops the copy |
| Report an end | the report (id `<runner>/completed`) | `end_reported` | The same |

## Records an older Runner left

A record says which rules it was written under (`rules`, now `RULES` = 2). Rules 1 are earlier builds of this branch,
which wrote `unregistered` and an end of `no-job` only on GitHub's answer, and an idle slot with an end of `no-job`
for one asking. A record without `rules` is main's, whose `unregistered` may stand for a 404 and whose stops for no
job were beliefs.

Restore reads an older record once, in one place (`Record::upgrade`), in two steps, and writes it under rules 2:

1. **Its beliefs are cleared, and the facts it lacks are read from what it holds.** Whether the VM came up: a job, a
   phase that waits for or runs one, or one past the boot that no failed start left (the vm-host's boot time adds to
   it at restore). Without GitHub's word of a job, main's `unregistered` of a standing machine is dropped, and so is
   that of a gone machine that came up, with its settled end: the removal is asked again and its answer decides. A
   standing machine main stopped for no job, or idle with `unregistered`, asks instead. Rules 1's idle slot with an
   end of `no-job` asks.
2. **GitHub's stored word is replayed through the lifecycle** (`Slot::hear`), as if heard now: its start, its end,
   and the registration gone under a machine that stands, read as an answer is: with no job named, GitHub's 204;
   under a job GitHub named, that job over.

A record of rules 2 is taken as it is. One whose facts disagree is a mistake of the Runner's: restore logs it as an
error and takes it as written, never through the older rules. `crates/daemon/tests/records.rs` reads every record
those Runners could have written, and `crates/daemon/tests/schedules.rs` leaves records in their formats amid
generated schedules.

## What stays open

- A 204 that is lost, to a crash before its write or to a dropped connection whose retry meets 404, makes the next
  answer report a job that did not run: one start and one end too many, never a job lost. So does a 204 main
  heard: its `unregistered` cannot be told from one it wrote on a 404. Telling the two apart needs a second source,
  such as the guest saying whether `Runner.Worker` ever ran.
- A machine that ran a job, and whose start and end both went with a session, is reported without GitHub's result.
  The App's `actions: read` permission lets the Runner read it from the job's REST view; it does not yet.
- GitHub removing an ephemeral registration that never ran a job, within the minutes of the wait, would read as a
  job. GitHub keeps offline registrations for far longer.
