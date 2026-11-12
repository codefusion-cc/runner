# Container mode

A host without virtualization (a VPS with no nested KVM, a plain Linux PC, a Mac that should not hold a kernel's
memory per job) runs each job in a container of its own instead of a machine. The setting is the host's:

```json
{ "sandbox": "container", "engine": "/run/user/1001/podman/podman.sock" }
```

Left out, `sandbox` is `vm` and nothing changes. `engine` is the Unix socket of any Docker-compatible engine: rootless
Podman (what `scripts/host-setup.sh --sandbox container` sets up), rootless Docker, OrbStack, or Docker with user
namespaces. A rootful engine that maps a container's root onto the host's is refused when the Runner starts.

A container's firewall is loaded in its own network namespace, which can load no kernel module: the host loads
`nf_tables` and `nft_ct` itself. `host-setup.sh` loads them and names them in
`/etc/modules-load.d/codefusion-runner.conf` for every boot; on another host, do the same.

## What a job gets

Everything above the hypervisor is the same in both modes: the vm-host, its states, its waits and its timeout, the
daemon, the budget. `cf-vmm-oci` stands where cloud-hypervisor stands, and a "machine" is one container:

- made from `codefusion-runner-guest:<version>`, which `image install --engine` put in the engine;
- never privileged, `no-new-privileges`, with nothing of the host's mounted: no folder, no device, never the
  engine's socket;
- held to its size's memory with no swap, its CPUs and 4096 processes, so a job that runs out ends `oom`; an engine
  that answers the create with any warning (a limit it could not set) gets its container removed and the job refused,
  and the supervisor refuses to start a job whose control group reads `max` for `memory.max` or `pids.max`;
- held to the size's scratch disk: the vm-host reads the container's writable layer (`SizeRw`) every 2 seconds, ends
  the job when it holds more, and reports its peak as the job's disk peak; where the engine's storage can bound a
  layer (overlay on xfs, an engine with root) the create also asks for `StorageOpt.size`;
- on a network of its own, which reaches no other job's container.

The container's first process is the supervisor, as the container's root with only `NET_ADMIN`, `SETGID`, `SETPCAP`
and `SETUID`. Before the job starts it locks the job in, and any step that fails ends the container instead:

1. the machine's firewall in the container's own network namespace: the internet, port 53 to the name servers the
   engine wrote in `resolv.conf` and nothing else of `BLOCKED`, of 127/8 off its own loopback, of the networks next
   to it (its interfaces, its gateway) or of the host's own; a rule set that does not load is a failed job, never an
   open one;
2. the memory and process limits read back from the control group, as above;
3. every capability out of the bounding set;
4. a seccomp filter on top of the engine's: no `unshare` or `clone` with `CLONE_NEWUSER`, no `clone3`, no x32 call
   and nothing of an architecture it does not know, so a job makes no user namespace and reaches none of the host
   kernel's code a namespace's capabilities open;
5. the image's user `runner`, with no capability in any set and not dumpable;
6. a read of `/proc/self/status` that proves it: that user, every capability set empty, `NoNewPrivs` and the filter
   on.

`no-new-privileges` is what lets the supervisor install the filter without `CAP_SYS_ADMIN`; it also means `sudo` in
a job does not work in container mode, as it never has.

A container ends at the job's timeout like a machine, and is removed with its volumes and its network when its
vm-host lets go. One whose vm-host is gone ends anyway:

- its input closes with the vm-host's attach (`StdinOnce`), and the supervisor takes that as its host gone;
- the supervisor ends the job itself at its longest life plus 30 seconds, whatever the host does;
- the daemon sweeps the engine at most every 30 seconds, from its tick, and removes the containers and networks whose
  vm-host is gone. A vm-host is known by its process id, its start time and the boot it ran in (the same owner a
  machine host's leftovers are judged by), so a reused process id keeps nothing alive.

Installing a new image removes the engine's older job images, keeping the one before it and any a container still
uses.

A container shares the host's kernel, so a container host is held to less than a machine host:

- it serves private repositories only (`public_repos` is refused);
- it gives no job Docker (`docker_repos` is refused): a `cf-docker` job runs only on a machine host;
- it is never `trusted`, so main's deploy, release and signing jobs stay on machines;
- it takes only jobs that ask for `cf-container`: the Runner adds that label to `labels` and `only_labels` itself.

## The image

A container-image release (`container-image-<stem>` for arm64, `amd64-container-image-<stem>` for x86_64) is the
machine image's root file system as a gzip tar, its kind signed in the manifest beside its hashes. A machine host
never picks one, and a container host refuses an `erofs` image. `image install --engine <socket>` verifies the
release, streams the joined parts into the engine while hashing them, and names the image only once size and hash
are the signed ones; on any failure it removes what it imported. `image.yml` publishes one next to each machine
image.

## Slots

The budget is the same as a machine host's, so a size's count is set by memory and cores alike:

| Cores | Memory | cf-2g | cf-4g | cf-6g | cf-8g |
|---|---|---|---|---|---|
| 12 | 11.7 GiB | 4 | 2 | 1 | 1 |
| 6 | 11.4 GiB | 2 | 2 | 1 | 1 |
| 4 | 7.6 GiB | 1 | 1 | 0 | 0 |

## Tests

- `crates/vmm-oci/tests/engine.rs`, against a real engine (CI's Linux jobs, with Docker and with rootless Podman): a
  job cannot read a host file or reach the engine's socket, reaches no listener of the host and no metadata address,
  makes no user namespace, is held to its memory, is ended at its timeout, two jobs at once do not see each other's
  files, a container whose vm-host was killed mid-job ends and goes at the daemon's next sweep, and a container-image
  release installed into the engine runs a job.
- `crates/supervisor/tests/container.rs`: the firewall's inputs and what it lets out, the limits check, the seccomp
  filter on each architecture (with a property test against its specification) and the look at the kernel's account.
- `crates/vmhost/src/driver.rs`: a container that writes past its disk is stopped, and its disk peak is reported.
- `crates/image/tests/store.rs`: the image goes whole into the engine, each host takes only its kind, and an import
  that stops early, cannot be named or is refused leaves nothing.
- `xtask/tests/host_setup.rs`: `--sandbox` and the keys it writes.
