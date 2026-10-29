# What GitHub's scale set service does, as seen

The Runner's tests run against `cf-fake-github`, and a fake that is kinder than GitHub hides bugs. This page holds
what was seen at GitHub, so each rule of the fake (`Options::auto_assign` in `crates/fake-github/src/github.rs`) can
be checked against it. `crates/scaleset/tests/github_rules.rs` checks the fake against each rule through the real
client.

## The probes

Probes run against `codefusion-cc/runner-probe`, a private repository kept for them: reuse it, with a scale set made
for the probe and removed at the end, instead of making a new repository.

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

Run 2026-10-17 the same way, by a Rust program, against `runner-probe` (then `runner-probe-held`) and a scale set made for the probe;
from K2 on, two more scale sets of the same labels polled with 0 beside it, as on a host that serves several. Each
call came 20 to 150 s after room appeared. A try counts only when the job was still held at the call: in about half
of them GitHub assigned it by itself first, 4 to 102 s after room appeared.

| Probe | What was done | What GitHub did |
|---|---|---|
| K2 | A job queued while the polls offered 0, then room, nothing else | Not assigned 706 s after room came |
| L1 | Scale set updated unchanged (`PATCH`) | Assigned 23 s and 29 s after the call; not in 120 s at the third try |
| L2 | Session refreshed (`PATCH`) | Assigned 14 s after the call, then 45 s after |
| L3 | Session deleted and opened again | Assigned 13 s after the call, then not in 120 s |
| L4 | An Actions runner of the scale set registered (`generatejitconfig`), never started | Not assigned in 120 s |
| M | REST `POST /repos/{owner}/{repo}/actions/runners/generate-jitconfig` with a name, `runner_group_id: 1` and labels | 201. The Actions runner carries exactly the labels given, no default ones, is online within seconds of starting (`ghcr.io/actions/actions-runner`), and its removal by the agents API (`DELETE .../pools/0/agents/{id}`) answers 204 while it is idle; the REST view says 404 after. Its process does not end by itself after the removal |
| N | Such an Actions runner started while the job was held, five tries | Each time GitHub looked 4 to 5 s after it came online and handed the job to the scale set, not to it; it was then removed with 204 |

Every count in every probe said `totalAvailableJobs: 0` while a job waited unassigned.

The Mac mini's log of the same day agrees: at 18:39 a slot freed while the other scale sets' newest polls offered 0,
and nothing was assigned until a job was queued elsewhere at 18:52:50, two seconds before a burst of assignments.

## The rules the fake keeps

| Rule | Evidence | Seen or inferred |
|---|---|---|
| GitHub looks at its queue when a job is queued, ends, is cancelled or taken back | E/F, I2, J2, the mini at 18:52:50 | Seen for a queued job and a take-back; inferred for an end and a cancel |
| It also looks by itself, at moments nobody can predict: within 30 s of room, or not in 3 minutes, or 13 minutes on the mini. The fake looks only when a test says so (`github_looks`) | A, G, I2, J1, J3, K, the mini 18:39 to 18:52 | Seen; the pattern behind the moments is unknown |
| No call to the scale set service makes it look every time: not a session opened, refreshed or opened again, not a scale set updated, not an Actions runner of a scale set registered | E, F, I1, J1 to J3, L1 to L4 | Seen; the assignments that followed a call came 13 to 45 s later, as GitHub's own looks do, and each call also failed once |
| An Actions runner registered outside any scale set makes it look as it comes online, and the job may go to a scale set rather than to it. The fake looks when a test says it came online (`runner_online`) | N | Seen five times of five |
| Such an Actions runner carries exactly the labels it was registered with, and takes a queued job of its repository whose labels it has | M | Seen for the labels; that it takes such a job is GitHub's documented meaning, and in N the scale set got the job first |
| A look gives each scale set with a session as many jobs as the smaller room of its last two polls leaves | E (the poll before the newest), J1 (the newest alone) | Which poll GitHub reads is inferred; the smaller of the two is no kinder than either reading |
| A session's opening hands out nothing | E, F | Seen |
| The counts never name a job GitHub holds | every probe | Seen |
| A session that ends or is forgotten takes its pending messages with it, and messages queued while there is none are lost | D kept one across a delete | GitHub was kinder in D; one sample is no promise, so the fake drops them |
| A refresh (`PATCH`) brings no counts | C | Seen |
| An assignment never acquired is taken back after 5 minutes as `JobCompleted` `canceled`, and queued again | J2, the mini at 18:57:52 | Seen; that it is queued again is inferred from J2, where it was handed out again at once |
| The REST view of an Actions runner says whether it is busy, and 404 once it is gone | H | Seen for an idle one; `busy: true` for a running job is GitHub's documented meaning, not probed. How fresh it is was not probed: the fake can answer one read behind (`set_rest_stale`), never from before the registration's own answer |

## What follows for the Runner

- No call to the scale set service makes GitHub hand over a job it holds. A Mac whose room comes back gets the job
  at GitHub's next look: the next job queued or ended in the repository, or GitHub's own, which can take many
  minutes (K2). So a job that waited two minutes while its scale set's polls showed room for as long gets an Actions
  runner of its own (`held.rs`): registered for its repository outside the scale set, with exactly the scale set's
  labels (M), in a slot the budget gives as to any job. Its arrival makes GitHub look (N); the job goes to the scale
  set or to it. Whichever gets it runs it. The other waits for nothing, so at the next look at the queue that finds
  no job it could take, or after five minutes, its registration's removal is asked for; GitHub answers 204 only for
  one that ran no job, and 422 for one that took it (`docs/job-state.md`). A scale set gets one at a time and one in
  five minutes at most, and a paused or full host none. It carries all the set's labels, so GitHub could give it a
  queued job of the repository that asks for only some of them and wants another size: while one waits, it gets
  none, and the log says why. The console's report names the held jobs per repository and
  label set (`heldJobs`).
- GitHub hands a job only to a scale set whose polls show room, and takes back one that waits in line unacquired after
  five minutes. A larger size on a Mac that keeps running smaller jobs would never show room, and its job never come.
  So the Runner makes room for it (`making.rs`): smaller sizes are held back while it waits, and its scale set offers a
  place in line while GitHub has the job back, or after its polls found no room for ten minutes.
- A message lost with a session cannot be asked for again. The Runner asks the REST view of each Actions runner it
  believes awaits a job before a session opens (`reconcile`), and before it removes the registration of a machine
  that ended unheard (`Service::sweep`). A removal's own answer is evidence too: 204 only for a registration that held
  no job, 422 under a job, 404 once a one-job Actions runner removed its own. What each answer proves, and what is
  written before what is done, is `docs/job-state.md`. The App has `actions: read`, so the result of a job whose
  `JobCompleted` was lost could be read from the job's REST view; the Runner does not read it yet.
- A scale set is made for each label set the workflows on a repository's default branch ask for, and for each one the
  jobs GitHub has queued or runs ask for that those workflows do not name: a pull request's new workflow or labels
  would otherwise wait for ever. The Runner reads them with `actions: read`, about once a minute
  (`GET /repos/{repo}/actions/runs?status=queued`, the same with `in_progress`, then `/actions/runs/{id}/jobs`): the
  oldest 10 of the newest 100 runs, whose jobs waited longest, and one page of each run's jobs. A listing names the ETag of its last answer, so a queue that did
  not change costs 304s, which GitHub does not count against the rate limit; the jobs of a run whose `updated_at`
  did not change are not asked for again, and those of one that did are asked for with their ETag. A job's labels
  pass the same rules as a workflow's (`LabelSet::served`), and such a set is served for 30 minutes after a job of
  it was last seen queued or running, 8 of a repository and 32 of a host at most, the newest first. On a public
  repository, a pull request's from a fork (its run's `head_repository` is another, or none) makes no set: it is a
  stranger's code, and runs only on the default branch's sets. Without the permission, or when GitHub does not
  answer, only the default branch's sets are served, the console hears why, and no set a queued job had is closed
  for it for up to 24 hours, the longest GitHub lets a job wait.
- Which job goes first is the Runner's to choose only among the jobs it holds: those that wait for room on the Mac,
  across its scale sets, and the offers of one message. Within one scale set, GitHub chooses which assigned job each
  Actions runner gets. A job message names its repository, its name, its run, the run's event and its workflow file at
  a ref (`jobWorkflowRef`, `owner/repo/.github/workflows/ci.yml@refs/heads/main`); it names no head branch and no run
  attempt. The one `JobAssigned` kept from the probes (`cf-scaleset`'s `types.rs`) shows none of them, perhaps only
  because it was cut short; a job whose message names none of them waits as a pull request's would (`turns.rs`). A job that should never wait behind pull requests runs on labels of its own.
