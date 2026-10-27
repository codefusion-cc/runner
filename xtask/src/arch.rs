//! The architectures a guest is built for. A host runs guests of its own architecture: the Mac and arm64 Linux
//! hosts aarch64, x86_64 Linux hosts x86_64. The kernel, the supervisor's target and the Linux monitor are pinned
//! for each.

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    /// Every architecture a pin is kept for, in the order checks report them.
    pub const ALL: [Arch; 2] = [Arch::Aarch64, Arch::X86_64];

    /// The architecture this xtask was built for, which is the host's.
    pub fn host() -> Result<Arch> {
        Arch::named(std::env::consts::ARCH)
    }

    /// The architecture by its Rust (and Alpine) name.
    pub fn named(name: &str) -> Result<Arch> {
        Arch::ALL
            .into_iter()
            .find(|arch| arch.name() == name)
            .ok_or_else(|| format!("no guest is pinned for {name}: aarch64 and x86_64 are").into())
    }

    /// Its name as Rust, Alpine and the image manifest write it.
    pub fn name(self) -> &'static str {
        match self {
            Arch::Aarch64 => "aarch64",
            Arch::X86_64 => "x86_64",
        }
    }

    /// The target the guest's supervisor is built for.
    pub fn musl_target(self) -> &'static str {
        match self {
            Arch::Aarch64 => "aarch64-unknown-linux-musl",
            Arch::X86_64 => "x86_64-unknown-linux-musl",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_others_are_refused() {
        for arch in Arch::ALL {
            assert_eq!(Arch::named(arch.name()).unwrap(), arch);
            assert!(arch.musl_target().starts_with(arch.name()));
        }
        for other in ["arm64", "amd64", "riscv64", ""] {
            assert!(Arch::named(other).is_err(), "{other}");
        }
        assert!(Arch::host().is_ok());
    }
}
