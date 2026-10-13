# What GitHub's scale set service does, as seen

The Runner's tests run against `cf-fake-github`, and a fake that is kinder than GitHub hides bugs. This page holds
what was seen at GitHub, so each rule of the fake (`Options::auto_assign` in `crates/fake-github/src/github.rs`) can
be checked against it. `crates/scaleset/tests/github_rules.rs` checks the fake against each rule through the real
client.

## The probes

Run 2026-10-12 against a throwaway repository with one `workflow_dispatch` workflow and one scale set made for the
probe, deleted at the end. A Python script spoke the protocol by hand: a session, long polls with a chosen
`X-ScaleSetMaxCapacity`, and no Actions runner ever ran a job. Every job was cancelled at the end. `cap` below is
the capacity the poll offered; it counts the jobs already assigned, as the Runner's does.

| Probe | What was done | What GitHub did |
|---|---|---|
| A | A job queued while the polls offered 0; then polls with room, same session | Assigned 144 s after it was queued, 82 s after room appeared, with no event in between |
| C | Session refreshed (`PATCH`) | Answered with `"statistics": null` |
| D | A job cancelled while no poll waited; session deleted and opened again | The `JobCompleted` (`canceled`) came on the new session's first poll; message ids went on across sessions |
| E | A job queued while the polls offered 0; session opened again 20 s later; the new session polled with room | Not assigned in 60 s. The job queued next (F) was assigned to a poll that offered 0, against the room of the poll before it |
| F | Session opened again after polls with room; the new session polled with no room beyond its assigned job | Not assigned |
| G | Polls with room, nothing else | Assigned 148 s after it was queued, 65 s after room appeared |
| H | REST `GET /repos/{owner}/{repo}/actions/runners/{id}` of a JIT registration | `{"busy": false, "status": "offline", ...}`; 404 once removed. The agents API showed no assigned request |
| I1 | Scale set updated unchanged after polls with room | Not assigned in 45 s |
| I2 | The same, a second job | Assigned 18 s after the update, 40 s after it was queued: an update, or GitHub's own look |
| J1 | An Actions runner registered and removed after polls with room | Assigned before the registration, 27 s after it was queued, against room only the newest poll offered |
| J2 | The same, a second job | Nothing for 105 s; then the job assigned in J1 and never acquired came back as `JobCompleted` `canceled` (taken back after 5 minutes), and both waiting jobs were assigned at once |
| J3 | The same, a third job | Not assigned in 3 minutes |
| K | A job queued 457 s while the polls offered 0, then room | Assigned 222 s after room came |

Every count in every probe said `totalAvailableJobs: 0` while a job waited unassigned.

The Mac mini's log of the same day agrees: at 18:39 a slot freed while the other scale sets' newest polls offered 0,
and nothing was assigned until a job was queued elsewhere at 18:52:50, two seconds before a burst of assignments.

## The rules the fake keeps

| Rule | Evidence | Seen or inferred |
|---|---|---|
| GitHub looks at its queue when a job is queued, ends, is cancelled or taken back | E/F, I2, J2, the mini at 18:52:50 | Seen for a queued job and a take-back; inferred for an end and a cancel |
| It also looks by itself, at moments nobody can predict: within 30 s of room, or not in 3 minutes, or 13 minutes on the mini. The fake looks only when a test says so (`github_looks`) | A, G, I2, J1, J3, K, the mini 18:39 to 18:52 | Seen; the pattern behind the moments is unknown |
| Nothing the Runner does makes it look: not a session opened, not a scale set updated, not an Actions runner registered | E, F, I1, J1 to J3 | Seen; I2 is the one case that may say otherwise, and J1 shows GitHub looks on its own at such moments |
| A look gives each scale set with a session as many jobs as the smaller room of its last two polls leaves | E (the poll before the newest), J1 (the newest alone) | Which poll GitHub reads is inferred; the smaller of the two is no kinder than either reading |
| A session's opening hands out nothing | E, F | Seen |
| The counts never name a job GitHub holds | every probe | Seen |
| A session that ends or is forgotten takes its pending messages with it, and messages queued while there is none are lost | D kept one across a delete | GitHub was kinder in D; one sample is no promise, so the fake drops them |
| A refresh (`PATCH`) brings no counts | C | Seen |
| An assignment never acquired is taken back after 5 minutes as `JobCompleted` `canceled`, and queued again | J2, the mini at 18:57:52 | Seen; that it is queued again is inferred from J2, where it was handed out again at once |
| The REST view of an Actions runner says whether it is busy, and 404 once it is gone | H | Seen for an idle one; `busy: true` for a running job is GitHub's documented meaning, not probed. How fresh it is was not probed: the fake can answer one read behind (`set_rest_stale`), never from before the registration's own answer |

## What follows for the Runner

- No call of the Runner makes GitHub hand over a job it holds. A Mac whose room comes back gets the job only at
  GitHub's next look: the next job queued or ended in the repository, or GitHub's own, which can take minutes.
- A message lost with a session cannot be asked for again. The Runner asks the REST view of each Actions runner it
  believes awaits a job before a session opens (`reconcile`), and before it removes the registration of a machine
  that ended unheard (`Service::sweep`). A removal's own answer is evidence too: 204 only for a registration that held
  no job, 422 under a job, 404 once a one-job Actions runner removed its own. What each answer proves, and what is
  written before what is done, is `docs/job-state.md`. The result of a job whose `JobCompleted` was lost cannot be read
  without the App permission `actions: read`.
- Which job goes first is the Runner's to choose only among the jobs it holds: those that wait for room on the Mac,
  across its scale sets, and the offers of one message. Within one scale set, GitHub chooses which assigned job each
  Actions runner gets. A job message names its repository, its name, its run, the run's event and its workflow file at
  a ref (`jobWorkflowRef`, `owner/repo/.github/workflows/ci.yml@refs/heads/main`); it names no head branch and no run
  attempt. The one `JobAssigned` kept from the probes (`cf-scaleset`'s `types.rs`) shows none of them, perhaps only
  because it was cut short; a job whose message names none of them waits as a pull request's would (`turns.rs`). A job that should never wait behind pull requests runs on labels of its own.
