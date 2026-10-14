# CodeFusion Runner

A Rust program for macOS and arm64 Linux that runs GitHub Actions jobs, each in its own small virtual machine that is
gone when the job ends. It runs CodeFusion's CI on CodeFusion's own machines.

The idea: a Mac opens a job slot only when it has the memory for it. The job gets a fixed amount of memory and CPU,
and when it ends the whole machine goes away, so an idle Mac holds nothing for CI.

Two programs here are called a runner. `codefusion-runner`, the Runner, is this one and runs on the Mac. The Actions
runner is GitHub's own program (`actions/runner`), which runs inside each virtual machine and talks to GitHub.

This repository is public and holds no secrets. It has no licence file: the code is published to be read and built,
not yet to be reused.

## State

The Runner takes jobs from GitHub and runs each in a machine of its own, as a service of the Mac. It has run so
against a fake GitHub and a fake console with real machines (`cargo xtask it --only service`), and on 2026-10-11
its first jobs from the real GitHub on a MacBook Air: one that passed, one that failed and one cancelled while it
ran, each in a machine that was gone when the job ended. What is there:

- the service (`codefusion-runner daemon`, kept running by launchd on a Mac and by systemd on Linux): it reads which repositories and `runs-on`
  labels to serve, keeps a scale set and a session at GitHub for each, and for every job GitHub assigns asks the
  budget for memory, starts a machine and registers an Actions runner for that one job. When the job ends, however
  it ends, the machine is destroyed, the registration removed, the memory given back, and the console told once,
- sizes from the label: `cf-2g`, `cf-4g`, `cf-6g`, `cf-8g`; `self-hosted` alone is 4 GB. A machine is counted as
  its size and 192 MB, of which what it already holds of the Mac's memory counts as given. A 16 GB Mac nobody
  works on runs two `cf-4g` at once when its own programs leave it about 10.4 GB available (twice 4.2 GB and its
  2 GB floor); one somebody works on runs them only while the person is away,
- nothing stays behind: a Runner that starts finds the machines of the one before, goes on with those that run and
  removes what is left of the others, on the Mac and at GitHub,
- never more machines than GitHub assigned jobs, and no machine stopped while its job runs. A machine idle for 10
  minutes, or found surplus, asks GitHub to remove its registration, and only GitHub's answer stops it: a removal
  means no job came, a refusal means one did, since a job's start can go missing with GitHub's messages. For the
  same reason a machine that came up and ended with no word of a job still counts as one for about two and a half
  minutes (the minute GitHub's word may come late, then the removal and its answer); in that time a newly queued
  job may wait for a machine that would have fit (`docs/job-state.md`),
- a job in its own virtual machine (`codefusion-runner vm`): one Linux machine per job with a fixed amount of memory
  and CPU, no swap and nothing that gives memory back while it runs. The job runs in a container in that machine as
  the image's user; its `sudo` makes it root of the container, not of the machine. When the job ends the machine and
  its disk are gone, and the end is named: `ok`, `failed`, `oom`, `timeout`, `cancelled`, an eviction, a lost guest
  or a boot that failed,
- the limits a job cannot move: its memory (a job that runs out is stopped inside its machine and ends `oom`, the
  Mac's memory stays where it was), and its time (a deadline, 20 s for the job to end by itself, then by force),
- what a job reaches: the internet, and names through the Mac's gateway. Not the Mac, the networks the Mac is in,
  other jobs' machines, private, tailnet and metadata addresses, or anything over IPv6,
- Docker for a job that asks for it with `cf-docker`, in a repository the Mac's `docker_repos` lists (private ones
  only): a daemon of the job's own in a sandbox beside its container, so `docker build`, `docker run` and
  `docker compose` work as on GitHub's runners. Its containers, privileged ones too, reach what the job reaches and
  nothing more, and are root of the sandbox, not of the machine. One Docker job at a time per Mac
  (`docker_max_jobs`); `docs/docker.md` says how and why,
- the guest: Alpine's `linux-virt` kernel and an initramfs whose PID 1 is the supervisor, built reproducibly by
  `cargo xtask guest` and embedded in the binary,
- images: an Ed25519 signature over `SHA256SUMS`, every hash and size checked, the root filesystem in parts below
  GitHub's 2 GB asset limit. `image install` verifies a release in a folder of its own before a machine boots from
  it, and refuses one older than the newest it has accepted,
- reports to the CodeFusion Console: the Mac's state every minute, and each job's start and end within a second
  or two. While the console is away up to 500 job events wait in a file and arrive later, each once. Jobs that
  wait while a machine of their size fits are in the report with how long and why, and warned of in the log after
  a minute.

`docs/MAP.md` says where things are and how a job's machine is put together.

## Run it on a Mac

The Runner is a launchd agent of the person logged in: no root, no password, nothing outside the home folder. It
takes no job between a start of the Mac and the log-in, so a Mac that runs jobs logs in by itself.

A configuration file says what the Mac is:

```json
{
  "machine": "mac-a-vm",
  "state_dir": "/Users/someone/Library/Application Support/codefusion-runner",
  "app_dir": "/Users/someone/.config/codefusion-runner",
  "image": "image-2026.10.10-21",
  "console": "https://console.example",
  "use": "dedicated"
}
```

- `machine` names the Mac at GitHub and at the console. Its scale sets and Actions runners carry it, so no other
  Runner may have the same name.
- `state_dir` holds everything the Runner keeps and is on the Mac's own disk. `app_dir` holds the GitHub App's
  `app.json` and `app.pem` (0600); `scripts/runner-app.ts` in `codefusion-cc/codefusion` makes the App and writes them.
- `use` is `dedicated` for a Mac nobody works on and `workstation` for someone's own.
- Optional: `repos` (serve only these), `public_repos` (public ones, on purpose), `deadline_minutes` (360),
  `github_api`, `build_folder` (see the update below), `docker_repos` (the private repositories whose `cf-docker`
  jobs get Docker) and `docker_max_jobs` (1). Without `console` the Mac reports to none.
- The file names where the App's key and the console's token go, so one that others can write is refused
  (`chmod go-w` it). Keep it in a folder that stays and is not `state_dir` (which `--purge` deletes): the agent
  reads it at every start.
- A running Runner takes up a changed file by itself, as an update does: it looks every 30 seconds, and when a
  Runner could start on the new file (it is read as above and its `image` is installed), it takes no new job until
  no machine stands, then starts again on it. The console's page says what it waits for. A file no Runner could start
  on (one that names a console before `enroll`, an image not installed, another `state_dir` or `machine`, which only
  `install` changes) changes nothing; a wait that outlasts the update's limit (50 minutes) gives up on that version
  of the file, and the Runner takes jobs again. Both stay on the console's page (`settingsError`) until the file
  changes. So `image install` the image first, then name it here; `scripts/docker-rollout-mac.sh` does both.

The Mac needs Rust to build the Runner (below) and nothing else. Images are public, so a Mac without `gh` fetches a
release with `curl -fLO https://github.com/codefusion-cc/runner/releases/download/<v>/<file>` for `Image`,
`image.json`, `initramfs.cpio.gz`, `SHA256SUMS`, `SHA256SUMS.sig` and every `rootfs.erofs.NNN`, into an empty folder.

```sh
codefusion-runner image install <folder> --version <v> --state-dir <state_dir>
codefusion-runner check --config <file> --github   # what is missing, and what GitHub shows this Mac
codefusion-runner enroll --config <file>           # the console's one-time code on standard input
codefusion-runner install --config <file>
```

`install` copies the program to `<state_dir>/bin`, writes `~/Library/LaunchAgents/cc.codefusion.vm-runner.plist` and
starts the agent. The Runner's log is `<state_dir>/logs/runner.log` (10 MB, then one older file beside it); what
launchd itself says is in `launchd.log` there.

What the log says between a start and a job, each once and again when it changes:

```
the Runner starts version="0.1.0+446125c0a1b2" machine=air-vm
a scale set at GitHub set=air-vm--cf-4g-177a1a95 id=10 repo=org/app labels=cf-4g size="cf-4g"
a session is open: the Runner listens for this scale set's jobs set=air-vm--cf-4g-177a1a95
the host is closed: a new job waits why="calming" opens_in_s=60
the host is open: a new job gets a machine
job started runner=air-vm-j1766ef2 job=build at=2026-10-11T09:54:18Z heard_after_s=41
```

A Mac opens after a minute in which nothing was wrong with it, and a job that comes sooner waits for that. `why`
is the budget's word for what closes it: `calming`, `memory-pressure`, `swapping`, `under-floor`, `thermal`,
`battery`, `disk`, `no-reading`, `paused` and so on. `job started` is written when GitHub says so, which may be most
of a minute after the start: `at` is the start by GitHub's time and `heard_after_s` how late its word came.

`version` prints the package's version and the commit the program was built at (`codefusion-runner
0.1.0+446125c0a1b2`, with `-dirty` after it when the sources it was built from differ from that commit), the text
the log's first line and the console have too. `check` ends its list with the memory at that moment: `now: 2.30 GiB
of memory available, 0.30 GiB above the floor of 2.00 GiB; no machine fits beside what runs`. That is the budget's
own arithmetic, so it says what a Runner on this Mac would decide, short of the calm minute.

`enroll` typed at a terminal asks for the code and does not show it while it is typed or pasted. The code is the one
the console's Add a machine shows, made for exactly this `machine`. Do not put it on a command line (`echo <code> |
…` stays in the shell's history); a script hands it over on standard input from where it keeps it.

The state folder (images, scratch disks, machine files, the Runner's copy) is marked as skipped by Time Machine, the way
`tmutil addexclusion <folder>` does it, by the daemon at every start and by `enroll`, `install` and `image install`. A
Mac that cannot set the mark logs a line and goes on. A cache folder the Runner makes later goes through
`cf_host_macos::exclude_from_backup` as it is made.

One Runner works over a state folder at a time: a second `daemon` over the same one ends at once and says so.

To update by hand, build the new version and run its `install` again. It refuses while a machine stands, so a job is
never cut off by an update. A Runner installed so updates itself when the console asks (`update`), from a checkout of its
own. The config's `build_folder` names that checkout, an absolute path not on an external volume, and the Runner must
have been built there. The checkout stays at a commit (`git checkout --detach`), not on a branch, and has no changes of
its own. Otherwise `update` touches nothing and says why: a person's checkout is never fetched into or moved. The
update fetches `main`, checks it out at its commit and builds it while jobs run. It takes no new job until no machine
stands and no slot promised to GitHub is left, then installs and starts again. A new program that does not stay up for
a minute three times is replaced by the one before, and the console hears why; a start that ends as asked (a signal,
`restart`) does not count. `restart` ends the Runner at once and launchd (systemd on Linux) starts it again; machines
that run go on and are adopted.

`uninstall` stops the agent and every machine, removes every scale set and registration at GitHub and says what it
could not remove; `--leave` also tells the console the Mac is gone, `--purge` deletes the state folder with its images.

## Run it on Linux

On Linux (arm64, KVM, systemd, cgroup v2; Debian 12 and Ubuntu 24.04 are known to work) the Runner is a unit of the
user's own systemd, `codefusion-runner.service`, run by a user of its own. Each job's vm-host runs in a systemd scope
of its own whose cgroup is delegated to it, so machines live through a restart or an update of the Runner and are
adopted. Nobody sits at such a host: `use` is `dedicated` or left out (`workstation` is refused), and the budget keeps half a
gigabyte for the host and runs machines on its fast cores.

Once, as root (Ubuntu 24.04 also needs the line about user namespaces):

```sh
sudo apt install passt git curl build-essential pkg-config libssl-dev
sudo useradd --create-home --home-dir /data/codefusion-runner --shell /bin/bash codefusion-runner
sudo usermod -aG kvm codefusion-runner
sudo loginctl enable-linger codefusion-runner
echo kernel.apparmor_restrict_unprivileged_userns=0 | sudo tee /etc/sysctl.d/60-codefusion-runner.conf && sudo sysctl --system
```

The home is where the images, the job disks and the build go, so it is on a disk with room (20 GB and more), not on
one mounted `noexec`. Lingering starts the user's systemd with the host and keeps it running without a login.

Then as that user (`sudo -iu codefusion-runner`): Rust from rustup, the sources in a checkout of the Runner's own, and

```sh
git clone https://github.com/codefusion-cc/runner ~/runner && cd ~/runner && git checkout --detach
cargo xtask build
```

The configuration is the Mac's without `use`, in `~/.config/codefusion-runner/config.json`, with the App's
`app.json` and `app.pem` beside it (`app_dir` `~/.config/codefusion-runner`), `state_dir`
`~/.local/state/codefusion-runner` and `build_folder` `~/runner`, each as an absolute path. Then, with the image
fetched as above:

```sh
target/release/codefusion-runner image install <folder> --version <v> --state-dir <state_dir>
target/release/codefusion-runner check --config <file> --github
target/release/codefusion-runner enroll --config <file>
target/release/codefusion-runner install --config <file>
```

`check` lists what Linux needs and the command that brings what is missing: `/dev/kvm` open to the user, lingering,
memory and pids handed on to the user's systemd, user namespaces, pasta, a state folder that may run programs, and a
build that carries cloud-hypervisor. `install` copies the program to `<state_dir>/bin`, writes
`~/.config/systemd/user/codefusion-runner.service` (below `$XDG_CONFIG_HOME` when the user's systemd has it set),
enables and starts it. Under `sudo -iu` the Runner finds the user's systemd in `/run/user/<uid>` by itself.

The log is `<state_dir>/logs/runner.log`, and what the Runner printed before it opened it is in `systemd.log` beside it.

```sh
systemctl --user status codefusion-runner
systemctl --user restart codefusion-runner
journalctl --user -u codefusion-runner     # when systemd started and stopped it
```

`uninstall`, `update` and `restart` work as on a Mac.

## Build and try it

You need an Apple silicon Mac and Rust (`rustup`).

```sh
cargo xtask build     # guest image, release binary, ad hoc signed with the virtualization entitlement
target/release/codefusion-runner boot-proof
```

The embedded kernel and initramfs are signed with a key made for that build, which only shows that the binary is
whole. Images a machine downloads are signed with the project's key, whose public half is
`crates/image/trusted-key.txt`.

Checks, as CI runs them:

```sh
cargo fmt --all --check
cargo xtask guest
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p cf-supervisor --target aarch64-unknown-linux-musl --bins -- -D warnings
CF_REQUIRE_EMBEDDED=1 cargo test --workspace
cargo deny check
```

and on an arm64 Linux host with pasta (`apt install passt`), OpenSSL's headers (`apt install pkg-config libssl-dev`)
and user namespaces (on Ubuntu 24.04, `sysctl kernel.apparmor_restrict_unprivileged_userns=0`), for the crates CI
lists in `LINUX_CRATES`, the service's among them:

```sh
cargo clippy -p cf-host-linux -p cf-vmm-ch -p cf-vmhost -p cf-daemon -p codefusion-runner --all-targets -- -D warnings
cargo test -p cf-host-linux -p cf-vmm-ch -p cf-vmhost -p cf-daemon -p codefusion-runner
cargo run -p cf-host-linux --example linux-probe
```

`cargo test -p cf-daemon --test scopes` starts a stand-in vm-host the way the service does, in a scope of the user's
systemd, and checks that it claims the scope's cgroup; without a systemd of the user's it says so and passes, unless
`CF_REQUIRE_USER_MANAGER` is set.

CI cannot boot a machine, so the jobs in real machines run on a Mac of your own, or on an arm64 Linux host with
KVM, in a cgroup delegated to you (`systemd-run --user --scope -p Delegate=yes cargo xtask it`):

```sh
gh release download <image tag> -R codefusion-cc/runner -D target/it/release
cargo xtask build && cargo xtask it
```

On Linux, `systemd-run --user --scope -p Delegate=yes cargo test -p cf-vmm-ch --test kvm` checks how a real
machine ends (a panic, a stop, a kill, an out-of-memory in its cgroup, a monitor that dies).

`it` installs nothing and calls neither GitHub nor a console: its `service` and `commands` scenarios run against
fakes of both on the Mac itself.
