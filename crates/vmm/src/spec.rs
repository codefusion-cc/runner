use std::path::PathBuf;

/// The smallest machine the guest kernel and the supervisor boot in.
pub const MIN_MEMORY: u64 = 256 << 20;

/// What a machine is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmSpec {
    /// The uncompressed arm64 kernel `Image`.
    pub kernel: PathBuf,
    /// The initramfs.
    pub initramfs: PathBuf,
    /// The machine's memory in bytes, fixed for its life: a multiple of 1 MiB, at least [`MIN_MEMORY`].
    pub memory_bytes: u64,
    /// The machine's CPUs, 1 to 64.
    pub cpus: usize,
    /// The disks, each found in the guest by its [`Disk::id`].
    pub disks: Vec<Disk>,
    /// The machine's network.
    pub network: Network,
    /// The file the guest's console is appended to.
    pub console: PathBuf,
}

/// One disk image attached to the machine as a virtio block device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// A raw disk image.
    pub path: PathBuf,
    /// The device's serial in the guest (`/sys/block/<dev>/serial`): 1 to 20 of `a-z`, `0-9` and `-`.
    pub id: String,
    /// Whether the guest may write to it. A read-only image may be attached to several machines at once.
    pub read_only: bool,
}

/// A machine's network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// No network device.
    None,
    /// One device behind the host's address translation: the guest reaches the internet and gets its address by
    /// DHCP; nothing reaches the guest.
    Nat,
}

/// Why a specification cannot be a machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    /// Memory under the minimum or not whole mebibytes.
    #[error("memory of {0} bytes: it must be a multiple of 1 MiB and at least 256 MiB")]
    Memory(u64),
    /// No CPU, or more than a machine can have.
    #[error("{0} CPUs: a machine has 1 to 64")]
    Cpus(usize),
    /// A disk id the guest could not carry as a serial.
    #[error("the disk id {0:?} is not 1 to 20 of a-z, 0-9 and -")]
    DiskId(String),
    /// Two disks with one id: the guest could not tell them apart.
    #[error("two disks have the id {0:?}")]
    DuplicateDisk(String),
}

impl VmSpec {
    /// Checks what can be checked without the files: sizes, counts and disk ids.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.memory_bytes < MIN_MEMORY || !self.memory_bytes.is_multiple_of(1 << 20) {
            return Err(SpecError::Memory(self.memory_bytes));
        }
        if !(1..=64).contains(&self.cpus) {
            return Err(SpecError::Cpus(self.cpus));
        }
        for (index, disk) in self.disks.iter().enumerate() {
            let valid = (1..=20).contains(&disk.id.len())
                && disk
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
            if !valid {
                return Err(SpecError::DiskId(disk.id.clone()));
            }
            if self.disks[..index].iter().any(|other| other.id == disk.id) {
                return Err(SpecError::DuplicateDisk(disk.id.clone()));
            }
        }
        Ok(())
    }
}
