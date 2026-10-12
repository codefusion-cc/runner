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
| Started | `github: Some`, `start_reported: true` | A `JobStarted`; REST `busy: true`, at any moment; REST 404 with no removal written down; a `DELETE` refused with 422 |
| Ended | `end_reported: true` after a completion report | A `JobCompleted`; or the machine gone and the wait for GitHub's word (`RESULT_WAIT`) over; or an uninstall, which waits for nothing |
| No job | `github: None`, `removing: Some(why)`, `end_reported: true` without a report | The Runner decided so and wrote it down first (below) |

Two ways lead to "No job":

- **Surplus.** The scaler retires an idle Actions runner past `IDLE_GRACE` only after REST says `busy: false`. It
  writes `removing: "surplus"`, then sends `DELETE`. A 422 means it took a job meanwhile: it is Started, and
  `removing` is cleared.
- **The verdict on a machine that ended unheard** (`ask_github`). The sweep asks REST once the wait is over.
  `busy: true` or 404 means a job ran. `busy: false` is evidence only for its moment. The sweep decides "no job" on
  a second `busy: false`, read at a later sweep than the first. Under a view one read behind, that answer is from
  the moment of the first read, after the machine was gone. Then it writes `removing: "no job"` and sends `DELETE`.

## The registration

`registering` (written before GitHub is asked), then registered (the record has `runner_id`), then `removing` (written
before `DELETE`), then `unregistered` (written after GitHub answered 204 or 404). The record is deleted once it is
unregistered, its end is reported and its machine is gone.

## The rules

1. **A count from before an event is never used after it.** GitHub's counts arrive with messages. The REST look
   at a session's opening (`reconcile`) runs before the session is opened, so the opening counts postdate every
   answer it acted on. Mid-session, an end comes either as a message, together with counts, or not at all. With no
   message, the session was lost, and the next opening counts afresh. The Runner's own count only ever lags behind
   GitHub's into fewer machines: a machine that went with its job unheard counts until it is settled
   (`lost_with_job`).
2. **Nothing is deleted at GitHub before the fact that it is being deleted, and why, is stored.** That fact is
   `removing` for an Actions runner the Runner gives up. A machine that is gone with its verdict stored needs no
   `removing`: `ended` and the job fields say why.
3. **A REST answer is evidence only for its moment.** A 404 after a `DELETE` of the Runner's own says nothing
   about a job. A `DELETE` answered 404 says nothing either, since the HTTP client retries a `DELETE` whose answer
   was lost. Only `busy: true`, a 422, or a 404 for a registration the Runner never set out to remove means a
   job started.
4. **An end is reported exactly once for every reported start, whichever path noticed it first.** Every start
   and end goes through `Machines` (`Events::job`, `settle`). A start reported for a record that was settled as
   "no job" opens its end again. No completion could have gone out before it, since a completion needs a
   reported start.
5. **The fake takes back an assignment not taken within five minutes**, as GitHub does (`docs/github-protocol.md`,
   probe J2): a `JobCompleted` `canceled` with no Actions runner, and the job is queued again.

## Order of writes and acts, and what may repeat

| Step | Written first | Then done | After a crash between them |
|---|---|---|---|
| Register | intent record (`registering`) | `generatejitconfig` | The sweep looks for the name and removes it |
| Start a machine | the record with its slot | the vm-host starts | No machine: the record ends as `daemon-restart` |
| Retire a surplus | `removing: "surplus"` | `DELETE`, then stop | Restore keeps the machine as done and the sweep sends `DELETE`. 204 or 404 stops the machine. 422 means Started |
| Verdict "no job" | `removing: "no job"` | `DELETE` | The sweep sends `DELETE` again; 404 means nothing |
| Report a start | the report (outbox, id `<runner>/started`) | `start_reported` | Reported again under the same id; the outbox drops the copy |
| Report an end | the report (id `<runner>/completed`) | `end_reported` | The same |
| Unregistered | `unregistered` | the record removed | Removed at the next settle |

Every REST read repeats safely. A `DELETE` repeats safely once its reason is stored.

## What stays open

- A job that starts and ends entirely between the REST read and the `DELETE` of a surplus Actions runner is not
  seen. Its start message is still heard while the session lives. The window is one round trip.
- A machine that ran a job, and whose start and end both went with a session, is reported without GitHub's result.
  The result can be read only with the App permission `actions: read`.
- GitHub removing an ephemeral registration that never ran a job, within the minutes of the wait, would read as a
  job. GitHub keeps offline registrations for far longer.
