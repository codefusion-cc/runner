# Docker jobs

A job with the label `cf-docker`, in a repository the Mac's `docker_repos` lists, gets a Docker daemon of its own.
`docker build`, `docker run` and `docker compose` (with BuildKit) work as on GitHub's runners, and the daemon is gone
with the job's machine. Workflows pick it the way they pick a size, from a variable:

```yaml
runs-on: ${{ vars.CI_RUNNER_DOCKER || 'ubuntu-latest' }}   # CI_RUNNER_DOCKER=cf-docker
```

`cf-docker` alone is a 4 GB machine; `[cf-docker, cf-6g]` asks for more. A public repository never gets Docker, even
listed: anyone can open a pull request there. A job that asks for `cf-docker` in a repository the Mac does not give
Docker is not offered to the Mac; the Mac's report says why.

## Where the daemon runs

Beside the job's container, in a sandbox the guest's PID 1 makes, not inside the container:

- The job's root has the machine's user ids: its user namespace maps every id to itself. So its container keeps
  `/proc/sys` read-only and masks `sysrq-trigger` and `kcore`. With those masked, the kernel mounts no new `/proc`
  below, and every container the daemon starts mounts one. Unmasked, the job's root would own the machine's settings.
- The sandbox's root is a shifted id, 1048576 (`cf_supervisor::docker::SHIFT`), and owns nothing of the machine's. An
  unmasked `/proc` gives it nothing: the kernel's settings, sysrq and kcore are the machine's root's, and only an id
  of the sandbox is ever mapped. The job's user and group and the `docker` group keep their own ids on both sides,
  so the job's files and the socket mean the same to both.
- The sandbox is a sibling of the job's container, not its child: the job's root has no rights over its namespaces,
  cannot see its processes and cannot trace them. Its view of the job's root is built as the machine's root before
  the job's container starts, so the job cannot change those paths under it.

What the sandbox sees: the job's root (the workspace, `/tmp` and every path a step names in `docker run -v`), its own
`/proc`, `/sys`, `/dev` (with `net/tun`), `/run`, and the daemon's data at `/var/lib/docker` on the job's disk. The
socket is `/run/cf-docker/docker.sock`, linked from `/run/docker.sock` and `/var/run/docker.sock`, group `docker`.

The daemon's network is a namespace the sandbox owns. pasta joins it to the machine's network, so its sockets are
the machine's and pass the guest's firewall like any other the job opens. Ports a container publishes are forwarded
from the machine's side, so a step reaches them at `localhost`. `--no-map-gw` keeps the gateway the gateway: without
it pasta would hand the gateway's address to the machine's loopback.

The daemon's network gets one rule before the daemon starts: TCP segments with no flag at all are dropped on the way
in. Ubuntu 24.04's pasta sends window updates without the ACK flag ([passt bug 84](https://bugs.passt.top/show_bug.cgi?id=84),
fixed upstream in 2024-03). A receiver ignores such a segment, but the daemon's NAT takes it for invalid and does not
map it to the container. The daemon's own kernel then answers it with a reset that ends the connection. When the
image's pasta has the fix, the rule can go.

## What a container reaches

What the job reaches: the internet, and names through the gateway. Not the Mac (the gateway, its LAN and tailnet
addresses), the networks the Mac is in, other jobs' machines, private, link-local, metadata and Tailscale addresses,
or anything over IPv6. That goes for a privileged container and one on the daemon's own network too: the firewall
is the guest's, in a network namespace that neither the job nor the sandbox owns.

A privileged container is root of the sandbox. It can mount, and change its own network, but it cannot write the
kernel's settings, trigger sysrq, read kcore or open the machine's disks.

## Limits

- Memory and processes: the daemon and every container count against the job's machine, under the job's control
  group. A job that runs out ends `oom` like any other; the Mac is not touched. A container may have limits of its
  own (`--memory`, Compose's `mem_limit`, `--cpus`); one stopped at its own limit exits 137 and the job goes on, since
  only the job's own limit makes the job `oom`. A Docker job's daemon and a small Compose stack hold about 750 MB.
- Disk: the job's 40 GB disk, gone with the machine; `/var/lib/docker` is on a cache disk where the host keeps
  caches (below), and on the job's disk where it does not.
- One Docker job at a time per Mac (`docker_max_jobs`, 1 to 8). It counts promised, waiting and running Docker jobs
  until their machine is gone, after a Runner restart too. A job GitHub assigns past the cap without being offered
  room still runs, as any assigned job does; the log says so.

## The cache

A host keeps one cache per repository: a 20 GB disk (sparse: it holds what was written) with the daemon's data,
images and build cache, so a job does not pull or build again what the one before did. `docker_cache_gb` (30) is
what all caches of a host may hold together; 0 keeps none and deletes those there were, and a host whose state
folder cannot share blocks between files (APFS can; on Linux btrfs and XFS can, ext4 cannot) keeps none either.

- Each Docker job gets a copy of its repository's cache that shares every block until it is written (APFS's
  clonefile, Linux's reflink), or an empty one the guest makes a file system on, writing no zeros, so a new cache
  holds of the host's disk what the job wrote. It writes only its copy.
- Once the job is over, the copy becomes the repository's cache only when everything says it may
  (`cf_daemon::caches::refusal`). GitHub said the job succeeded, on a push, a schedule or a manual dispatch. The
  workflow is the repository's own, at `refs/heads/<its default branch>`. GitHub's REST API says the run checked
  out that branch of this repository (read while the job runs; a workflow's ref is where its file is, and a reusable
  workflow at the default branch called from another branch runs that branch's code). The guest said the daemon still answered,
  stopped by itself and let the disk go. Anything else is thrown away: a pull request, a fork, another branch, a
  failed or cancelled job, a job GitHub never said the end of, a run that could not be read, a disk let go
  uncleanly. So a cache holds what the
  default branch built and nothing a pull request, a fork or another repository left. A cache's key is a hash of
  its repository's name in lowercase, and its record names the repository, which is checked before it is used.
- The job's end hook removes its containers, volumes and networks from the job's daemon, by its socket, whatever
  context the job chose; images and build cache stay. A next job starts with none of the containers of the one
  before. A job the host stopped (its deadline, a cancellation) is never kept, so its daemon is not waited for.
- Before a cache is mounted the image's `e2fsck` checks it; one that does not mount is made anew and the job runs on
  an empty one (`replaced`). A cache the daemon could not run on is deleted (`dropped`). Over the budget the cache
  used longest ago goes (`evicted`), never one a job holds now.
- The console's Runners page shows each host's caches with their size and last write, and what last happened to
  them (`kept`, `discarded` with why, `replaced`, `dropped`, `evicted`); the report's `dockerCaches` and
  `dockerCacheEvents`.

## How it fails

- The daemon does not start (the image lacks a tool, a module is missing): the job runs without Docker, and its
  start hook writes an error on the run, "Docker did not start", with the end of the daemon's log, as soon as the
  sandbox ended or after 60 seconds without an answer.
- The daemon stops during a job: the steps that use it fail; the log is at `/run/cf-docker/dockerd.log`.

## Tests

- `crates/supervisor/tests/decisions.rs`: a container stopped at its own limit is not the job's out of memory
  (`oom::OwnLimit`).
- `crates/supervisor/tests/docker.rs`: the sandbox's ids (every id once, none the machine's root, no overlap, the
  job's ids kept) and the commands of the daemon, pasta and the rule.
- `crates/daemon/tests/docker.rs`: the daemon flag reaches the machine's request and record, the cap with a promise,
  a running machine, one assigned past the cap and a restart, and a Docker scale set that acquires one job only.
- `crates/workflows/tests/labels.rs`, `crates/daemon/src/names.rs`, `crates/runner/src/config.rs`: the label, the
  repositories that get Docker, the settings.
- `crates/daemon/tests/caches.rs`: the rule (each condition refusing alone, and a property: nothing but a successful
  run of the repository's own workflow on its default branch with a clean disk is kept), the key (one per
  repository in any case, two repositories never one), eviction (least recently used first, none in use), and the
  store on a real file system: kept, thrown away, replaced, dropped, a record naming another repository, a copy in
  use or whose machine never started, sweeping after a restart, a budget of 0, and what a store reads back.
- `crates/daemon/tests/docker.rs`: the same through the daemon: a push to main keeps its copy and the next job starts
  from it; a pull request, another branch, a failure, a cancellation, another repository's workflow, an unclean disk,
  a job GitHub never said the end of, a reusable workflow at main run from another branch and a run never read do
  not; a copy outlives a restart (records read back from disk). `crates/scaleset/tests/github_rules.rs`: a run says
  the branch its code came from, not its workflow's.
- `cargo xtask it --only docker-cache`, against real machines: a first job fills an empty cache and leaves it clean,
  the next one on a copy has its image, none of its containers or volumes, and builds from the cache, and a disk of
  garbage is made anew. It goes red without the end hook's tidying, without the daemon's own stop, and when the
  guest never reuses a cache.
- `cargo xtask it --only docker`, against real machines: a Compose stack built and published, a bind mount of `/tmp`,
  and the probes from plain, privileged and host-network containers. The target that shows the firewall at work is
  a port the test opens on the host itself: through pasta a closed port and a dropped packet look alike. The
  scenario goes red with the guest's firewall emptied (the open port is reached through the gateway and the LAN
  address), with `--no-map-gw` taken out (names through the gateway fail), and with the sandbox's root unshifted (a
  privileged container writes the machine's `core_pattern` and triggers sysrq). `cargo xtask it --only docker-oom`:
  a container that takes all of the job's memory ends the job `oom`.
