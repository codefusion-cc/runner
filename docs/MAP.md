# Map

| Path | What |
| --- | --- |
| `crates/image` (`cf-image`) | The guest image: `Manifest`, `verify` (signature, hashes, sizes, version), `write_release`, and the embedded kernel and initramfs (`build.rs` reads `target/guest`). Tests in `tests/verify.rs`, `tests/embedded.rs`. `trusted-key.txt` is the public key of the image signing key. |
| `crates/vmm-vz` (`cf-vmm-vz`) | Virtualization.framework through `objc2-virtualization`. M0: `boot_to_ready`. M1 turns it into the `Vmm` implementation. |
| `crates/supervisor` (`cf-supervisor`) | PID 1 of the guest, a static `aarch64-unknown-linux-musl` binary. M0: mounts, loads modules, says `ready`, powers off. |
| `crates/runner` (`codefusion-runner`) | The binary. M0: `version`, `image verify`, `boot-proof`. `runner.entitlements` is signed into it. |
| `crates/budget` (`cf-budget`) | Admission, with no I/O: `Budget` (`observe`, `claim`, `confirm`, `adopt`, `usage`, `release`, `status`), the pure `admit`, `Settings` (every number, `for_host` has the defaults), `Size` and its labels, and the `Host` trait with `HostReading`. Tests: `admission`, `levels`, `races`, `properties`. |
| `crates/host-macos` (`cf-host-macos`) | `MacHost` reads macOS behind `cf_budget::Host` (page counts, pressure, swap, idle time, thermal, power, disk); `Raw` and `reading` are the arithmetic; `FakeHost` is the host tests script. `examples/host-probe.rs` prints a Mac's readings. |
| `crates/lifecycle` (`cf-lifecycle`) | `Slot`: the state machine of one slot and its job (events in, effects out, watchdogs, `saved` and `restore` for a daemon restart), and `StartBreaker`. `tests/model.rs` searches every state it can reach. |
| `crates/scaleset` (`cf-scaleset`) | The scale set protocol: `ScaleSetClient` (scale sets, sessions, one-job JIT configs, runner removal; `runner_by_name`; admin token kept until 120 s before it expires by GitHub's clock, replaced once on 401), `MessageSession` (long poll, ack, acquire), `listen` with the `Scaler` trait (handle, then ack; duplicates acked only; shutdown interrupts only the wait), `Backoff` and `supervise`. A token goes only to the API's origin and to hosts of `ACTIONS_HOST`: an address the service hands back that is elsewhere is refused. A registration whose answer was lost, or that names no runner, is removed again. Every status and malformed body is an `Error`. `examples/github_probe.rs` is the real check against a throwaway repository (prints no token). Tests in `tests/`, run against `cf-fake-github`. |
| `crates/daemon` (`cf-daemon`) | The Go scaler ported: `Scaler` (reserve before acquire, release for jobs taken elsewhere, converge on assigned jobs, retire surplus after `IDLE_GRACE`; a slot promised to a job nobody assigned goes back after `PROMISE_TTL` and when the scaler is dropped; a completion without new counts lowers the number of runners to have), `Roster` (per scale set: a message marks only its own set's runners), `names` (runner and scale set names, `ServePolicy`). The machine's slots (`Slots`, M2's budget) and VMs (`Hosts`, M1) are traits whose doc comments in `src/traits.rs` are the contract the two must meet; `testing` has stand-ins. Tests: `tests/scaler.rs`, `tests/listening.rs`. |
| `crates/ghapp` (`cf-ghapp`) | The GitHub App client: `AppCredentials::load` (`app.json` + `app.pem`, which must be 0600), the App JWT (iat now-60 s, exp now+9 min, clock skew learned from the `Date` header), installation tokens (cached, one request at a time), repositories, workflow files (the caller names the largest file to read), repository variables, registration tokens; `Http::new(timeout, retry)` with retries (429 and 5xx with jitter, `Retry-After`), HTTPS only (or this machine itself), redirects only to the same origin, at most `MAX_BODY_BYTES` of an answer and 100 pages of a listing. No error and no `Debug` carries an answer's text. Tests in `tests/app.rs`, run against `cf-fake-github`. |
| `crates/workflows` (`cf-workflows`) | `self_hosted` / `self_hosted_with`: the label sets a repository's workflows ask for (port of the Go `workflows` package, tests in `tests/labels.rs`), `size_gb` for the `cf-2g/4g/6g/8g` labels, `self-hosted` = 4 GB, the `vars.*` expressions evaluated with the repository's variables, and `MAX_FILE_BYTES`, the largest workflow file read. |
| `crates/secret` (`cf-secret`) | `Secret<T>`: a value whose `Debug` prints `Secret(..)`. Every token and key is wrapped in it. |
| `crates/fake-github` (`cf-fake-github`) | A stateful fake GitHub over real sockets, for tests only: App JWT checks, tokens, repositories, scale sets, sessions, long poll, acquire, JIT configs, with `Options::auto_assign` (the real service hands a queued job to a poll that has room), with faults (`fail`, `before`, `lose_answer`, `lose_session`, `expire_session_token`, `revoke_admin_tokens`, `send_the_service_to`, `send_the_queue_to`, clock offset). `harness` is what every crate's tests start from: the fake with the real App and scale set clients in front of it, `TestClock`, `fast` retries, `until`, and `elsewhere` (a host no token may reach). |
| `crates/vmm`, `guest-proto`, `console-client` | Empty until their milestone (M1 to M4); each file says which. |
| `xtask` | `cargo xtask keygen | guest | release | build`. Kernel package pinned in `xtask/src/kernel.rs`. |
| `image/Dockerfile` | The root filesystem a job runs in: GitHub's Actions runner image plus tools. Packed by `.github/workflows/image.yml`. |
| `.github/workflows` | `ci.yml` (macos-14: fmt, guest, clippy, tests, release build; ubuntu: cargo deny), `image.yml` (the image: built and verified on every change to it, signed and published from main, weekly), `automerge.yml`. |

## Run

- Build and boot proof: `cargo xtask build && target/release/codefusion-runner boot-proof`. The binary needs
  the virtualization entitlement (the build adds it, ad hoc) and an Apple silicon Mac.
- The guest supervisor builds with `rust-lld` (see `.cargo/config.toml`), so no cross toolchain is needed.
- What the budget sees on a Mac: `cargo run -p cf-host-macos --example host-probe`. It only reads and prints; compare
  with `vm_stat` and `sysctl kern.memorystatus_vm_pressure_level vm.swapusage`.
- GitHub's hosted Macs cannot boot VMs (no nested virtualization), so CI never runs `boot-proof`; it runs on a Mac.

## Image signing

Ed25519 over `SHA256SUMS` as unpadded base64url in `SHA256SUMS.sig` (Magnetar's scheme). The list names `image.json`
and every file `image.json` names, and nothing else. `image.json` carries the version, each file's size and hash, the
hash of the whole root filesystem and when the release was built, so all of that is signed with it.

`verify` checks the signature first (strictly: no weak key, no altered signature), then that the list and the manifest
agree, then every byte. It answers for the moment it read the files: use them from a folder only the Runner can write
to. `verify` also takes the build time of the newest image the machine has accepted and refuses an older one, so an
old release offered again under its old name does not come back. Nothing stores that time yet: `image verify` passes
0, and the Runner keeps it from M1 on.

The key signs image lists only. Anything else that gets signed later, such as the Runner's own updates, gets a key of
its own, so a list signed for one purpose never passes for another.

`xtask guest` signs the embedded bundle with a key made for that build, written to `target/guest/bundle-key.txt`; the
embedded self-check catches a damaged binary only. The trusted key never signs a bundle, which has no root filesystem
and no build time; `xtask release` signs nothing without a root filesystem. Downloaded images are checked against
`trusted-key.txt`.

### Where the key is, and a new one

The secret half is the secret `IMAGE_SIGNING_KEY` of the repository's `image` environment, which only `main` may
deploy to. `image.yml`'s `publish` job is the only one that names that environment. It checks out and compiles
nothing: it runs the signer and the verifier that the `build` job of the same run made on main, with the key on
standard input. Pull requests run the `build` job, which has no secret.

To replace the key (it leaked, or its holder changes):

1. `cargo xtask keygen <file>` writes the new secret half to `<file>` and prints the public half.
2. `gh secret set IMAGE_SIGNING_KEY --env image < <file>`, then delete `<file>`.
3. Put the public half into `crates/image/trusted-key.txt` and merge. The next image is signed with the new key.

A Runner built before step 3 refuses images signed with the new key and keeps the image it has until it is updated;
one built after it refuses every image signed with the old key. There is one trusted key at a time on purpose.

### The kernel

`xtask/src/kernel.rs` pins Alpine's `linux-virt` package by SHA-256; a build fetches it over HTTPS and refuses any
other bytes. The file says how the pin was checked against Alpine's own signature and how to move it.
