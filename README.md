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

The parts exist and are tested, each on its own. The daemon that joins them into a service comes next, so nothing
here takes a job from GitHub by itself yet. What works:

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
- admission by memory (`cf-budget`), the life of a slot (`cf-lifecycle`), GitHub's scale set protocol and the scaler
  (`cf-scaleset`, `cf-daemon`), the GitHub App client (`cf-ghapp`),
- `codefusion-runner boot-proof`, which boots the embedded guest in a 1 GB machine and waits for `ready`.

`docs/MAP.md` says where things are and how a job's machine is put together.

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
