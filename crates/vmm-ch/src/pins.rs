// The cloud-hypervisor a machine runs on Linux, pinned for each architecture. `cargo xtask cloud-hypervisor` fetches
// the host's into `target/vmm-programs`, the build script of cf-vmm-ch embeds the one of the architecture it builds
// for, and the Runner unpacks and checks it before every machine. This file is also read by that build script and by
// xtask, so it holds constants and plain data only.
//
// Each is the project's own static build, read from our copy (a release of this repository tagged
// `cloud-hypervisor-<version>`, holding both files) first and from the project's release only when the copy is
// missing. Our copy: `gh release create cloud-hypervisor-<version> cloud-hypervisor-static-aarch64
// cloud-hypervisor-static --prerelease --latest=false`, by the owner, after checking each file against the project's
// release.
//
// pasta is not pinned here: it comes with the system (`apt install passt`), see `programs::pasta`.

/// cloud-hypervisor's release, the same for every architecture.
pub const CLOUD_HYPERVISOR_VERSION: &str = "v53.0";

/// One architecture's static build of that release.
pub struct CloudHypervisorPin {
    /// The architecture, as Rust names it (`CARGO_CFG_TARGET_ARCH`, `std::env::consts::ARCH`).
    pub arch: &'static str,
    /// The asset's name in the release.
    pub asset: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// Every pinned build.
pub const CLOUD_HYPERVISOR: [CloudHypervisorPin; 2] = [
    CloudHypervisorPin {
        arch: "aarch64",
        asset: "cloud-hypervisor-static-aarch64",
        size: 5_468_264,
        sha256: "f192b510eea1c710cbc439d716bb0573c223fc463dbe3e6523788a2b7ef62850",
    },
    CloudHypervisorPin {
        arch: "x86_64",
        asset: "cloud-hypervisor-static",
        size: 7_062_256,
        sha256: "448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc",
    },
];

/// The pinned build for an architecture, if there is one.
pub fn cloud_hypervisor_for(arch: &str) -> Option<&'static CloudHypervisorPin> {
    CLOUD_HYPERVISOR.iter().find(|pin| pin.arch == arch)
}
