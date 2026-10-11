# CodeFusion Runner

A Rust program for macOS that runs GitHub Actions jobs, each in its own small virtual machine that is gone when the job
ends. It replaces the runner manager in `codefusion-cc/codefusion` on the CodeFusion Macs.

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

- the service (`codefusion-runner daemon`, kept running by launchd): it reads which repositories and `runs-on`
  labels to serve, keeps a scale set and a session at GitHub for each, and for every job GitHub assigns asks the
  budget for memory, starts a machine and registers an Actions runner for that one job. When the job ends, however
  it ends, the machine is destroyed, the registration removed, the memory given back, and the console told once,
- sizes from the label: `cf-2g`, `cf-4g`, `cf-6g`, `cf-8g`; `self-hosted` alone is 4 GB. A machine is counted as
  its size and 192 MB. A 16 GB Mac nobody works on runs two `cf-4g` at once; one somebody works on runs them only
  while the person is away,
- nothing stays behind: a Runner that starts finds the machines of the one before, goes on with those that run and
  removes what is left of the others, on the Mac and at GitHub,
- a job in its own virtual machine (`codefusion-runner vm`): one Linux machine per job with a fixed amount of memory
  and CPU, no swap and nothing that gives memory back while it runs. The job runs in a container in that machine as
  the image's user; its `sudo` makes it root of the container, not of the machine. When the job ends the machine and
  its disk are gone, and the end is named: `ok`, `failed`, `oom`, `timeout`, `cancelled`, an eviction, a lost guest
  or a boot that failed,
- the limits a job cannot move: its memory (a job that runs out is stopped inside its machine and ends `oom`, the
  Mac's memory stays where it was), and its time (a deadline, 20 s for the job to end by itself, then by force),
- what a job reaches: the internet, and names through the Mac's gateway. Not the Mac, the networks the Mac is in,
  other jobs' machines, private, tailnet and metadata addresses, or anything over IPv6,
- the guest: Alpine's `linux-virt` kernel and an initramfs whose PID 1 is the supervisor, built reproducibly by
  `cargo xtask guest` and embedded in the binary,
- images: an Ed25519 signature over `SHA256SUMS`, every hash and size checked, the root filesystem in parts below
  GitHub's 2 GB asset limit. `image install` verifies a release in a folder of its own before a machine boots from
  it, and refuses one older than the newest it has accepted,
- reports to the CodeFusion Console: the Mac's state every minute, and each job's start and end within a second
  or two. While the console is away up to 500 job events wait in a file and arrive later, each once.

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
  Runner and no cf-runners manager may have the same name.
- `state_dir` holds everything the Runner keeps and is on the Mac's own disk. `app_dir` holds the GitHub App's
  `app.json` and `app.pem` (0600).
- `use` is `dedicated` for a Mac nobody works on and `workstation` for someone's own.
- Optional: `repos` (serve only these), `public_repos` (public ones, on purpose), `deadline_minutes` (360),
  `github_api`. Without `console` the Mac reports to none.
- The file names where the App's key and the console's token go, so one that others can write is refused
  (`chmod go-w` it). Keep it in a folder that stays and is not `state_dir` (which `--purge` deletes): the agent
  reads it at every start.
- Beside the cf-runners manager, `app_dir` is the manager's own `~/.config/codefusion-runners` (with an `s`), which
  the Runner only reads, and `machine` is the manager's name with `-vm`.

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
the Mac is closed: a new job waits why="calming" opens_in_s=60
the Mac is open: a new job gets a machine
job started runner=air-vm-j1766ef2 job=build at=2026-10-11T09:54:18Z heard_after_s=41
```

A Mac opens after a minute in which nothing was wrong with it, and a job that comes sooner waits for that. `why`
is the budget's word for what closes it: `calming`, `memory-pressure`, `swapping`, `under-floor`, `thermal`,
`battery`, `disk`, `no-reading`, `paused` and so on. `job started` is written when GitHub says so, which may be most
of a minute after the start: `at` is the start by GitHub's time and `heard_after_s` how late its word came.

`version` prints the package's version and the commit the program was built at (`codefusion-runner
0.1.0+446125c0a1b2`), the text the log's first line and the console have too. `check` ends its list with the memory
at that moment: `now: 2.30 GiB of memory available, 0.30 GiB above the floor of 2.00 GiB; no machine fits beside
what runs`. That is the budget's own arithmetic, so it says what a Runner on this Mac would decide, short of the
calm minute.

`enroll` typed at a terminal asks for the code and does not show it while it is typed or pasted. The code is the
`--code` value of the command the console's Add a Mac shows for the cf-runners manager, made for exactly this
`machine`. Do not put it on a command line (`echo <code> | …` stays in the shell's history); a script hands it over
on standard input from where it keeps it.

One Runner works over a state folder at a time: a second `daemon` over the same one ends at once and says so.

To update, build the new version and run its `install` again. It refuses while a machine stands, so a job is never
cut off by an update. `uninstall` stops the agent and every machine, removes every scale set and registration at
GitHub and says what it could not remove; `--leave` also tells the console the Mac is gone, `--purge` deletes the
state folder with its images.

Beside the cf-runners manager of `codefusion-cc/codefusion` the Runner touches nothing of it: another launchd label,
another folder, another name at GitHub (it refuses to run under the manager's own). Both serve a `runs-on` that includes
`self-hosted`, so GitHub gives such a job to whichever has room first. A `runs-on: cf-4g` alone goes only to the
Runner.

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

CI's Macs cannot boot a machine, so the jobs in real machines run on a Mac of your own:

```sh
gh release download <image tag> -R codefusion-cc/runner -D target/it/release
cargo xtask build && cargo xtask it
```

`it` installs nothing and calls neither GitHub nor a console: its `service` and `commands` scenarios run against
fakes of both on the Mac itself.
