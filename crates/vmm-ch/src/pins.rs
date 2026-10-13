// The programs a machine runs on Linux, pinned. `cargo xtask vmm-programs` fetches and builds them into
// `target/vmm-programs`, the build script of cf-vmm-ch embeds them, and the Runner unpacks and checks them before
// every machine. This file is also read by that build script, so it holds constants only.
//
// cloud-hypervisor is the project's own static aarch64 build, read from our copy (a release of this repository
// tagged `cloud-hypervisor-<version>`) first and from the project's release only when the copy is missing. Our
// copy: `gh release create cloud-hypervisor-<version> cloud-hypervisor-static-aarch64 --prerelease
// --latest=false`, by the owner, after checking the file against the project's release.
//
// pasta (of passt) has no static aarch64 build upstream: the build makes one (`make static`) from the pinned
// source snapshot, read from our copy (`passt-<version>`) first and from passt.top only when it is missing. pasta
// is GPL-2.0-or-later and ships as a program of its own beside the Runner, with the offer of its source in the
// release notes.

/// cloud-hypervisor's release.
pub const CLOUD_HYPERVISOR_VERSION: &str = "v53.0";
/// The size of `cloud-hypervisor-static-aarch64` of that release.
pub const CLOUD_HYPERVISOR_SIZE: u64 = 5_468_264;
/// Its SHA-256.
pub const CLOUD_HYPERVISOR_SHA256: &str = "f192b510eea1c710cbc439d716bb0573c223fc463dbe3e6523788a2b7ef62850";

/// passt's snapshot, whose `pasta` the build makes.
pub const PASST_VERSION: &str = "2026_09_25.df90211";
/// The size of `passt-<version>.tar.xz`.
pub const PASST_SIZE: u64 = 296_112;
/// Its SHA-256.
pub const PASST_SHA256: &str = "cdf655b5677ce219108cf3df93a55e0ae98a376887be110fe35303ea900d5d68";
