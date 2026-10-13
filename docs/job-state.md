# What the Runner believes about a machine and its job

Every Actions runner the Runner registers has one record (`crates/daemon/src/records.rs`). The record holds what the
Runner believes about the job, and the console hears that belief. Three sources can move the belief, and each can be
stale or lost:

- **GitHub's session messages** (`JobStarted`, `JobCompleted`). They are lost with a session that ends or is
  forgotten, and arrive up to a minute late.
- **GitHub's REST view of the Actions runner** (`GET .../actions/runners/{id}`: `busy`, or 404). An answer is a
  snapshot of one moment, which may lie before the moment it was asked. It never lies before the registration's own
  answer: GitHub gave the id.
- **The Runner's own record.** A crash keeps only what was written. Whatever was in memory is lost.

The machine (vm-host) adds one fact: whether its guest came up. A guest that never came up ran no job.

## The job, per Actions runner

| State | Stored as | Moved by |
|---|---|---|
| Unknown | `github: None`, `start_reported: false` | Nothing yet: the machine stands, or it ended and no word came |
| Started | `github: Some`, `start_reported: true`, and the lifecycle's step in the same write | A `JobStarted`; REST `busy: true` or gone, at any moment; a `DELETE` answered 404 or refused with 422 |
| Ended | `end_reported: true` after a completion report | A `JobCompleted`; or the machine gone and the wait for GitHub's word (`RESULT_WAIT`) over; or an uninstall, which waits for nothing |
| No job | `github: None`, `unregistered: true`, `end_reported: true` without a report | GitHub answered the Runner's own `DELETE` with 204 |

Only a 204 to the Runner's own `DELETE` says no job ran: GitHub refuses to remove an Actions runner that holds a job
(422), and a one-job Actions runner removes its own registration once its job is over (then `DELETE` meets 404). An
intention of the Runner's is never evidence, and nothing it wants to do stops GitHub from being asked.

Three ways lead to a `DELETE`, all sent by the sweep (`Service::sweep`), the one place that removes a registration:

- **Surplus.** The scaler retires an idle Actions runner past `IDLE_GRACE` (`Hosts::retire`). Nothing is written:
  a Runner that dies first retires it again.
- **The idle limit.** A machine idle past `limits.idle` is not stopped on that belief, since its start may have gone
  with a session: the lifecycle asks for the registration's removal (`Effect::Unregister`) and stays idle. 204
  (`Event::Unregistered`) stops it as `no-job`, the one way a slot ends so; 422 or 404 makes it Started. Stopped
  from the console before the answer, it ends as `stopped` and is asked about once it is gone.
- **A machine that ended unheard** (`ask_github`): gone with no word of a job, after its guest may have come up
  (anything but a slot last written booting, or one that ended as never ready). The sweep asks REST once the wait
  is over. `busy: true` or gone
  means a job ran. `busy: false` is evidence only for its moment: on a second `busy: false`, read at a later sweep,
  the sweep sends `DELETE`, and the answer decides.

A start heard while a removal is pending voids it.

## The registration

`registering` (written before GitHub is asked), then registered (the record has `runner_id`), then `unregistered`
(written in the step that heard GitHub's 204 or 404, together with the machine's stop). The record is deleted once it
is unregistered, its end is reported and its machine is gone.

## The rules

1. **A count from before an event is never used after it.** GitHub's counts arrive with messages. The REST look
   at a session's opening (`reconcile`) runs before the session is opened, so the opening counts postdate every
   answer it acted on. Mid-session, an end comes either as a message, together with counts, or not at all. With no
   message, the session was lost, and the next opening counts afresh. The Runner's own count only ever lags behind
   GitHub's into fewer machines: a machine that went with its job unheard counts until it is settled
   (`lost_with_job`).
2. **Facts that must change together are one write, and a restart reads one fact.** GitHub's word of a job and the
   lifecycle step it causes are one write (`Events::job`). `unregistered` and the machine's stop are one write; a
   machine restored with `unregistered` gets GitHub's answer again (`Event::Unregistered`), whatever phase was
   written. Every write agrees with itself (`Record::agrees`): a job GitHub named ran; no slot boots, retries or
   waits beside it; none runs a job GitHub said is over; and no end without a job stands beside a job or before
   GitHub's 204.
3. **A word of a job is never dropped for the phase it meets.** The lifecycle answers every phase and event, with no
   arm for the rest. A start or an end heard while the machine boots means it was up; heard before there is a VM,
   it is kept and applied once one is up; heard while the machine is stopped, or after its slot is over, it is kept
   as the fact that a job ran.
4. **A REST answer is evidence only for its moment.** `busy: false` says nothing of a moment after it. A registration
   that is gone, by REST or by a `DELETE`'s 404, means a job ran, unless a 204 to the Runner's own `DELETE` was heard
   first; the HTTP client retries a `DELETE` whose answer was lost, so a lost 204 reads as a 404 (below).
5. **An end is reported exactly once for every reported start, whichever path noticed it first.** Every start
   and end goes through `Machines` (`Events::job`, `settle`). A start reported for a record that was settled as
   "no job" opens its end again. No completion could have gone out before it, since a completion needs a
   reported start.
6. **The fake takes back an assignment not taken within five minutes**, as GitHub does (`docs/github-protocol.md`,
   probe J2): a `JobCompleted` `canceled` with no Actions runner, and the job is queued again. **Its removals fail
   until GitHub recovers**, not for a count of tries the client's retries use up in milliseconds.

## Order of writes and acts, and what may repeat

| Step | Written first | Then done | After a crash between them |
|---|---|---|---|
| Register | intent record (`registering`) | `generatejitconfig` | The sweep looks for the name and removes it |
| Start a machine | the record with its slot | the vm-host starts | No machine: the record ends as `daemon-restart` |
| Hear a start | `github`, `start_reported` and the lifecycle, one write | the report | Reported again under the same id |
| Remove a registration | nothing | `DELETE` | Retired or asked again; a 204 whose write was lost reads as a job (below) |
| Hear a removal | `unregistered` and the stop, one write | the machine stops | Restore gives a machine whose record says `unregistered` GitHub's answer again |
| Report a start | the report (outbox, id `<runner>/started`) | `start_reported` | Reported again under the same id; the outbox drops the copy |
| Report an end | the report (id `<runner>/completed`) | `end_reported` | The same |
| Unregistered | `unregistered` | the record removed | Removed at the next settle |

Every REST read repeats safely, and so does a `DELETE`: what it proves is read from its answer, not from a reason
stored before it.

## Records an older Runner left

A record says which rules it was written under (`rules`, now `RULES` = 1; a record without it is the Runner of
main's or an earlier build's). Restore reads such a record once, in one place (`Record::upgrade`), and writes it
under these rules; `crates/daemon/tests/records.rs` reads every combination those Runners could have written. Their
`removing` and `ask_github` are not evidence of anything: under these rules nothing the Runner meant to do is.

| Written by an older Runner | Why it is not trusted | Read as |
|---|---|---|
| GitHub's start or end beside a lifecycle that boots, retries, waits or runs (two writes, a crash between) | The lifecycle lags behind GitHub's word | Consistent: running, or finishing once GitHub said the job is over |
| A standing machine stopping or killed for `no-job` or `stopped`, its registration not removed (the idle limit, a surplus removal after 204 or 404, a console stop) | A stop on the belief that no job came | Asks: idle, the registration's removal asked for at once, no stop before GitHub's answer |
| A standing machine with `unregistered` and no word of a job | Main wrote `unregistered` on a 404 too | Asks, as above |
| A machine gone with no word of a job, settled or not, with `unregistered` or not | Main settled it without asking, and removed it on 404 as on 204 | Asks: as one that ended unheard, its job reported if GitHub says one ran |
| A stop for the Mac's sake (memory, disk, sleep), a timeout, a lost guest, a failed start | A decision, not a belief about a job | Consistent; asked about once gone, like any other |
| A registration with no machine | No machine, no job | Consistent: removed by the sweep |

Asking costs a console stop that a Runner died in the middle of: the machine stays until GitHub answers, and runs on
if GitHub says it holds a job.

## What stays open

- A 204 that is lost, to a crash before its write or to a dropped connection whose retry meets 404, makes the next
  look report a job that did not run: one start and one end too many, never a job lost. So does a 204 an older
  Runner heard: its `unregistered` cannot be told from one it wrote on a 404. Telling the two apart needs
  a second source, such as the guest saying whether `Runner.Worker` ever ran.
- A machine that ran a job, and whose start and end both went with a session, is reported without GitHub's result.
  The result can be read only with the App permission `actions: read`.
- GitHub removing an ephemeral registration that never ran a job, within the minutes of the wait, would read as a
  job. GitHub keeps offline registrations for far longer.
