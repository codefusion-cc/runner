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

Milestone M0, the scaffold. What works:

- the Cargo workspace with its crates (most are empty until their milestone),
- the guest image: Alpine's `linux-virt` kernel and an initramfs with a small PID 1, built reproducibly by
  `cargo xtask guest` and embedded in the binary,
- the signed manifest and its verification: an Ed25519 signature over `SHA256SUMS`, every hash and size checked on
  load, and the root filesystem in parts below GitHub's 2 GB asset limit,
- `codefusion-runner boot-proof`, which boots the embedded guest in a 1 GB virtual machine and waits for `ready`.

It does not yet run a job. The plan has the milestones after this one; `docs/MAP.md` says where things are.

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
CF_REQUIRE_EMBEDDED=1 cargo test --workspace
cargo deny check
```
