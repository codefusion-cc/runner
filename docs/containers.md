# Container mode

A host without virtualization (a VPS with no nested KVM, a plain Linux PC, a Mac that should not hold a kernel's
memory per job) runs each job in a container of its own instead of a machine. The setting is the host's:

```json
{ "sandbox": "container", "engine": "/run/user/1001/podman/podman.sock" }
```

Left out, `sandbox` is `vm` and nothing changes. `engine` is the Unix socket of any Docker-compatible engine: rootless
Podman (what `scripts/host-setup.sh --sandbox container` sets up), rootless Docker, OrbStack, or Docker with user
namespaces. A rootful engine that maps a container's root onto the host's is refused when the Runner starts.

## What a job gets

Everything above the hypervisor is the same in both modes: the vm-host, its states, its waits and its timeout, the
daemon, the budget. `cf-vmm-oci` stands where cloud-hypervisor stands, and a "machine" is one container:

- made from `codefusion-runner-guest:<version>`, which `image install --engine` put in the engine;
- run as the image's user `runner`, every capability dropped, `no-new-privileges`, never privileged;
- with nothing of the host's mounted: no folder, no device, never the engine's socket;
- held to its size's memory with no swap, its CPUs and 4096 processes, so a job that runs out ends `oom`;
- on a network of its own, which reaches no other job's container;
- ended at the job's timeout like a machine, and removed with its volumes and its network when its vm-host lets go.

The next vm-host removes what one that died left: containers and networks whose owner is gone, on start and after
each job.

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
  job cannot read a host file or reach the engine's socket, is held to its memory, is ended at its timeout, two jobs
  at once do not see each other's files, a vm-host that crashed leaves nothing the next one does not remove, and a container-image
  release installed into the engine runs a job.
- `crates/image/tests/store.rs`: the image goes whole into the engine, each host takes only its kind, and an import
  that stops early, cannot be named or is refused leaves nothing.
- `xtask/tests/host_setup.rs`: `--sandbox` and the keys it writes.
