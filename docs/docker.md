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

The daemon's own networks, its bridge and the pools Compose and `docker network create` take from, are /16 blocks of
private ranges that share no address with anything next to the machine: its network, its gateway and the networks
the host is in (`cf_supervisor::docker::networks`). Docker's own default, 172.17.0.0/16, is taken only when it is
clear. A host with Docker of its own is on 172.17.0.0/16 itself; with that as the daemon's bridge, the host's address
would be the bridge's, inside the machine, and a packet to it would never reach the firewall. When no block is
clear, the job runs without Docker and is told why.

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
- Disk: the job's 40 GB disk, `/var/lib/docker` included, gone with the machine.
- One Docker job at a time per Mac (`docker_max_jobs`, 1 to 8). It counts promised, waiting and running Docker jobs
  until their machine is gone, after a Runner restart too. A job GitHub assigns past the cap without being offered
  room still runs, as any assigned job does; the log says so.
- Nothing is cached between jobs yet: each job pulls its images.

## How it fails

- The daemon does not start (the image lacks a tool, a module is missing): the job runs without Docker, and its
  start hook writes an error on the run, "Docker did not start", with the end of the daemon's log, as soon as the
  sandbox ended or after 60 seconds without an answer.
- The daemon stops during a job: the steps that use it fail; the log is at `/run/cf-docker/dockerd.log`.

## Tests

- `crates/supervisor/tests/decisions.rs`: a container stopped at its own limit is not the job's out of memory
  (`oom::OwnLimit`).
- `crates/supervisor/tests/docker.rs`: the sandbox's ids (every id once, none the machine's root, no overlap, the
  job's ids kept), the daemon's networks (clear of a host on Docker's own bridge, of a host in all of
  172.16.0.0/12, none when nothing is clear, and a property: never an address of what is next to the machine), and
  the commands of the daemon, pasta and the rule.
- `crates/daemon/tests/docker.rs`: the daemon flag reaches the machine's request and record, the cap with a promise,
  a running machine, one assigned past the cap and a restart, and a Docker scale set that acquires one job only.
- `crates/workflows/tests/labels.rs`, `crates/daemon/src/names.rs`, `crates/runner/src/config.rs`: the label, the
  repositories that get Docker, the settings.
- `cargo xtask it --only docker`, against real machines: a Compose stack built and published, a bind mount of `/tmp`,
  and the probes from plain, privileged and host-network containers. The target that shows the firewall at work is
  a port the test opens on the host itself: through pasta a closed port and a dropped packet look alike. The
  scenario goes red with the guest's firewall emptied (the open port is reached through the gateway and the LAN
  address), with `--no-map-gw` taken out (names through the gateway fail), and with the sandbox's root unshifted (a
  privileged container writes the machine's `core_pattern` and triggers sysrq). It also fails when an address it
  probes is in one of the daemon's own networks, where a probe would never leave the machine; it goes red with the
  bridge put on the host's LAN. `cargo xtask it --only docker-oom`:
  a container that takes all of the job's memory ends the job `oom`.
