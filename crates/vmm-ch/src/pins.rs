// The cloud-hypervisor a machine runs on Linux, pinned. `cargo xtask cloud-hypervisor` fetches it into
// `target/vmm-programs`, the build script of cf-vmm-ch embeds it, and the Runner unpacks and checks it before every
// machine. This file is also read by that build script, so it holds constants only.
//
// It is the project's own static aarch64 build, read from our copy (a release of this repository tagged
// `cloud-hypervisor-<version>`) first and from the project's release only when the copy is missing. Our copy:
// `gh release create cloud-hypervisor-<version> cloud-hypervisor-static-aarch64 --prerelease --latest=false`, by
// the owner, after checking the file against the project's release.
//
// pasta is not pinned here: it comes with the system (`apt install passt`), see `programs::pasta`.

/// cloud-hypervisor's release.
pub const CLOUD_HYPERVISOR_VERSION: &str = "v53.0";
/// The size of `cloud-hypervisor-static-aarch64` of that release.
pub const CLOUD_HYPERVISOR_SIZE: u64 = 5_468_264;
/// Its SHA-256.
pub const CLOUD_HYPERVISOR_SHA256: &str = "f192b510eea1c710cbc439d716bb0573c223fc463dbe3e6523788a2b7ef62850";
