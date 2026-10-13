//! Which of the host's CPUs a machine's vCPUs run on.
//!
//! On a board with big and little cores (an RK3588 has four Cortex-A76 and four Cortex-A55), a vCPU that the host
//! moves from one kind of core to the other can make `KVM_RUN` fail: KVM refuses to run a vCPU on a CPU its PMU does
//! not cover. So every machine is held to one kind of core: the host's fast cores as `cf_host_linux::fast_cpus`
//! names them, those of them this process may use, or the cores `--host-cpus` names.

use std::fmt;

/// A kernel CPU list (`0-3,6,8-9`, as `cpuset.cpus` and `/sys/devices/system/cpu/online` write them) as the CPU
/// numbers in order. An empty text is an empty list.
pub fn parse_list(text: &str) -> Result<Vec<u32>, String> {
    cf_host_linux::parse::cpu_list(text).ok_or_else(|| {
        format!(
            "{:?} is not a CPU list as the kernel writes one, such as 4-7 or 0,2-3",
            text.trim()
        )
    })
}

/// A list of CPU numbers as the kernel writes one: ranges where they run on.
pub struct List<'a>(pub &'a [u32]);

impl fmt::Display for List<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut cpus = self.0.to_vec();
        cpus.sort_unstable();
        cpus.dedup();
        let mut index = 0;
        while index < cpus.len() {
            let first = cpus[index];
            let mut last = first;
            while index + 1 < cpus.len() && cpus[index + 1] == last + 1 {
                index += 1;
                last = cpus[index];
            }
            if first > cpus[0] {
                f.write_str(",")?;
            }
            if first == last {
                write!(f, "{first}")?;
            } else {
                write!(f, "{first}-{last}")?;
            }
            index += 1;
        }
        Ok(())
    }
}

/// The cores machines run on: `chosen` (from the configuration) when given, which must be CPUs this process may
/// use (`allowed`), else the host's `fast` cores that it may use. `fast` is `None` when the host's CPUs could not be
/// read. Never empty.
pub fn host_cores(chosen: Option<&[u32]>, allowed: &[u32], fast: Option<&[u32]>) -> Result<Vec<u32>, String> {
    let mut cores = match chosen {
        Some(chosen) => {
            if let Some(cpu) = chosen.iter().find(|cpu| !allowed.contains(cpu)) {
                return Err(format!(
                    "CPU {cpu} was chosen for the machines, but this process may only use {}",
                    List(allowed)
                ));
            }
            chosen.to_vec()
        }
        None => {
            let fast = fast.ok_or("the host's CPUs could not be read from /sys/devices/system/cpu")?;
            let usable: Vec<u32> = fast.iter().copied().filter(|cpu| allowed.contains(cpu)).collect();
            if usable.is_empty() && !fast.is_empty() {
                return Err(format!(
                    "this process may use CPUs {}, none of the host's fast cores {}: name the machines' cores with \
                     --host-cpus",
                    List(allowed),
                    List(fast)
                ));
            }
            usable
        }
    };
    cores.sort_unstable();
    cores.dedup();
    if cores.is_empty() {
        return Err("no CPU is left for the machines".into());
    }
    Ok(cores)
}

#[cfg(target_os = "linux")]
pub(crate) mod host {
    //! What the kernel says about the CPUs this process may use.

    use super::parse_list;
    use std::path::Path;

    /// The CPUs this process may run on: its affinity, within what `cpuset.cpus.effective` of `cgroup` allows when
    /// that file is there.
    pub fn allowed(cgroup: &Path) -> Result<Vec<u32>, String> {
        // SAFETY: `set` is a plain bit set that sched_getaffinity fills; the size passed is its own.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above; pid 0 is this thread.
        if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) } != 0 {
            return Err(format!("sched_getaffinity: {}", std::io::Error::last_os_error()));
        }
        let mut cpus: Vec<u32> = (0..libc::CPU_SETSIZE as u32)
            // SAFETY: CPU_ISSET reads one bit of the set, below its size.
            .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu as usize, &set) })
            .collect();
        if let Ok(text) = std::fs::read_to_string(cgroup.join("cpuset.cpus.effective")) {
            let effective = parse_list(&text)?;
            cpus.retain(|cpu| effective.contains(cpu));
        }
        Ok(cpus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const RK3588_FAST: &[u32] = &[4, 5, 6, 7];

    #[test]
    fn a_list_is_read_as_the_kernel_writes_it_and_nothing_else() {
        assert_eq!(parse_list("0-3,6,8-9\n"), Ok(vec![0, 1, 2, 3, 6, 8, 9]));
        assert_eq!(parse_list(""), Ok(vec![]));
        for text in [
            "3-1", "a", "1,,2", "-1", "1-", "1 2", "0x3", "+1", ",", "6,4", "4-7,5",
        ] {
            let error = parse_list(text).unwrap_err();
            assert!(error.contains("such as 4-7"), "{text:?}: {error}");
        }
    }

    #[test]
    fn the_fast_cores_this_process_may_use_are_taken() {
        let all: Vec<u32> = (0..8).collect();
        assert_eq!(host_cores(None, &all, Some(RK3588_FAST)), Ok(vec![4, 5, 6, 7]));
        assert_eq!(host_cores(None, &[0, 1, 5, 7], Some(RK3588_FAST)), Ok(vec![5, 7]));
        // A host whose cores are alike has them all fast.
        assert_eq!(host_cores(None, &[0, 1], Some(&[0, 1, 2, 3])), Ok(vec![0, 1]));
    }

    #[test]
    fn little_cores_alone_are_never_taken_in_place_of_the_fast_ones() {
        let error = host_cores(None, &[0, 1, 2], Some(RK3588_FAST)).unwrap_err();
        assert!(
            error.contains("0-2") && error.contains("4-7") && error.contains("--host-cpus"),
            "{error}"
        );
    }

    #[test]
    fn a_host_whose_cpus_cannot_be_read_takes_none_unless_they_are_chosen() {
        assert!(host_cores(None, &[0, 1], None).is_err());
        assert_eq!(host_cores(Some(&[1]), &[0, 1], None), Ok(vec![1]));
    }

    #[test]
    fn a_choice_must_be_allowed_and_never_empty() {
        assert_eq!(
            host_cores(Some(&[6, 4]), &[0, 4, 6], Some(RK3588_FAST)),
            Ok(vec![4, 6])
        );
        let refused = host_cores(Some(&[4, 9]), &[0, 4], Some(RK3588_FAST)).unwrap_err();
        assert!(refused.contains("CPU 9"), "{refused}");
        assert!(host_cores(Some(&[]), &[0], Some(RK3588_FAST)).is_err());
        assert!(host_cores(None, &[], Some(&[])).is_err());
    }

    proptest! {
        #[test]
        fn a_written_list_reads_back_the_same(cpus in proptest::collection::vec(0u32..4096, 0..40)) {
            let mut expected = cpus.clone();
            expected.sort_unstable();
            expected.dedup();
            prop_assert_eq!(parse_list(&List(&cpus).to_string()), Ok(expected));
        }

        #[test]
        fn the_cores_taken_are_fast_and_allowed_and_every_such_core_is_taken(
            fast in proptest::collection::btree_set(0u32..16, 0..16),
            allowed in proptest::collection::btree_set(0u32..16, 0..16),
        ) {
            let fast: Vec<u32> = fast.into_iter().collect();
            let allowed: Vec<u32> = allowed.into_iter().collect();
            let both: Vec<u32> = fast.iter().copied().filter(|cpu| allowed.contains(cpu)).collect();
            match host_cores(None, &allowed, Some(&fast)) {
                Ok(cores) => prop_assert_eq!(cores, both),
                Err(_) => prop_assert!(both.is_empty()),
            }
        }
    }
}
