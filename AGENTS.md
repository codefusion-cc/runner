# Working in runner

The CodeFusion Runner: Rust, macOS (Apple silicon) first. One VM per job through Virtualization.framework, GitHub's
scale set protocol for jobs, the CodeFusion Console for rules and reports. The plan and its milestones live with the
owner's notes; read `docs/MAP.md` before exploring and update it when you move or add something it names.

This repository is **public**: no secrets, no keys, no personal e-mail, no hostnames of the owner's machines in any
commit, log or test. The image signing key exists only as the secret `IMAGE_SIGNING_KEY` of the `image` environment.

## Names

Ours is the Runner, `codefusion-runner`. GitHub's program inside the VM is the Actions runner, always with
"Actions" in front. A bare "runner" in code or docs is a mistake: say which one.

## Rules

- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace -j4` and
  `cargo deny check` are clean before a push. On the maintainer's Mac Rust is keg-only:
  `export PATH="/opt/homebrew/opt/rustup/bin:$PATH"`. At most 2 to 4 jobs at once; the Macs also run CI.
- One job per crate, named `cf-<job>`. Pure decision code (`budget`, `lifecycle`) does no I/O. What a call can meet at
  run time (a bad download, a hostile input) is a `Result`; only a programming mistake panics.
- Tests come from the spec and the ways it fails, not from the code; break the code on purpose and watch them go red.
  Mock only network, time and randomness.
- No `unsafe` without a `SAFETY:` comment naming why the call is sound.
- Workflows name every action by its commit, with the version as a comment. The one exception is the organization's
  own automerge action, which `automerge.yml` takes from `codefusion-cc/codefusion`'s main, as every repository does.
  A job that holds a secret checks out and compiles nothing: it runs what a job without secrets built.
- Branches are `feat/`, `fix/`, `docs/` and so on. No attribution lines in commits or PR bodies.
- Once checks pass and CI is green, a session adds the label `automerge` (`gh pr edit <n> --add-label automerge`);
  leave it off a draft or a pull request that must wait.

## Image

`cargo xtask guest` builds the kernel and initramfs into `target/guest`; `cf-image`'s build script embeds them.
A change to `crates/image` must keep `crates/image/tests/verify.rs` red-able: change the check, watch a test fail.
`cargo mutants -p cf-image -f crates/image/src/verify.rs` leaves no mutant alive that matters.
