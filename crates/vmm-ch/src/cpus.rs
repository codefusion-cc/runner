//! Which of the host's CPUs a machine's vCPUs run on.
//!
//! On a board with big and little cores (an RK3588 has four Cortex-A76 and four Cortex-A55), a vCPU that the host
//! moves from one kind of core to the other can make `KVM_RUN` fail: KVM refuses to run a vCPU on a CPU its PMU does
//! not cover. So every machine is held to one kind of core, the fastest the Runner may use, and every job gets the
//! same, fast cores. The kernel says how fast each core is in `cpu_capacity` (1024 for the fastest); a host whose
//! cores are all alike says nothing there, or the same for each, and every core it allows is taken.

use std::fmt;

/// The highest CPU number a list may name: far above any host, low enough that a hostile list costs nothing.
pub const MAX_CPU: usize = 4095;

/// A kernel CPU list (`0-3,6,8-9`, as `cpuset.cpus` and `/sys/devices/system/cpu/online` write them) as sorted,
/// distinct CPU numbers. An empty text is an empty list.
pub fn parse_list(text: &str) -> Result<Vec<usize>, String> {
    let text = text.trim();
    let mut cpus = Vec::new();
    if text.is_empty() {
        return Ok(cpus);
    }
    let number = |part: &str| -> Result<usize, String> {
        let valid = !part.is_empty() && part.len() <= 4 && part.bytes().all(|byte| byte.is_ascii_digit());
        match part.parse::<usize>() {
            Ok(cpu) if valid && cpu <= MAX_CPU => Ok(cpu),
            _ => Err(format!("{part:?} is not a CPU number from 0 to {MAX_CPU}")),
        }
    };
    for part in text.split(',') {
        let (first, last) = match part.split_once('-') {
            Some((first, last)) => (number(first)?, number(last)?),
            None => {
                let cpu = number(part)?;
                (cpu, cpu)
            }
        };
        if first > last {
            return Err(format!("the range {part:?} runs backwards"));
        }
        cpus.extend(first..=last);
    }
    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

/// A list of CPU numbers as the kernel writes one: ranges where they run on.
pub struct List<'a>(pub &'a [usize]);

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

/// The cores a machine runs on: of the CPUs this process may use (`allowed`), those of the highest capacity among
/// them. `capacity` gives a CPU's `cpu_capacity`, `None` where the kernel says nothing. When no allowed CPU has a
/// capacity, every allowed CPU is taken: the host's cores are all alike.
pub fn fastest(allowed: &[usize], capacity: impl Fn(usize) -> Option<u32>) -> Vec<usize> {
    let known: Vec<(usize, u32)> = allowed
        .iter()
        .filter_map(|&cpu| capacity(cpu).map(|value| (cpu, value)))
        .collect();
    let Some(top) = known.iter().map(|&(_, value)| value).max() else {
        let mut all = allowed.to_vec();
        all.sort_unstable();
        all.dedup();
        return all;
    };
    let mut cores: Vec<usize> = known
        .into_iter()
        .filter(|&(_, value)| value == top)
        .map(|(cpu, _)| cpu)
        .collect();
    cores.sort_unstable();
    cores.dedup();
    cores
}

/// The cores machines run on: `chosen` (from the configuration) when given, which must be CPUs this process may
/// use, else [`fastest`]. Never empty.
pub fn host_cores(
    chosen: Option<&[usize]>,
    allowed: &[usize],
    capacity: impl Fn(usize) -> Option<u32>,
) -> Result<Vec<usize>, String> {
    let cores = match chosen {
        Some(chosen) => {
            if let Some(cpu) = chosen.iter().find(|cpu| !allowed.contains(cpu)) {
                return Err(format!(
                    "CPU {cpu} was chosen for the machines, but this process may only use {}",
                    List(allowed)
                ));
            }
            let mut chosen = chosen.to_vec();
            chosen.sort_unstable();
            chosen.dedup();
            chosen
        }
        None => fastest(allowed, capacity),
    };
    if cores.is_empty() {
        return Err("no CPU is left for the machines".into());
    }
    Ok(cores)
}

#[cfg(target_os = "linux")]
pub(crate) mod host {
    //! What the kernel says about this host's CPUs.

    use super::parse_list;
    use std::path::Path;

    /// A CPU's `cpu_capacity`, `None` where the kernel has none.
    pub fn capacity(cpu: usize) -> Option<u32> {
        std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The CPUs this process may run on: its affinity, within what `cpuset.cpus.effective` of `cgroup` allows when
    /// that file is there.
    pub fn allowed(cgroup: &Path) -> Result<Vec<usize>, String> {
        // SAFETY: `set` is a plain bit set that sched_getaffinity fills; the size passed is its own.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above; pid 0 is this thread.
        if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) } != 0 {
            return Err(format!("sched_getaffinity: {}", std::io::Error::last_os_error()));
        }
        let mut cpus: Vec<usize> = (0..libc::CPU_SETSIZE as usize)
            // SAFETY: CPU_ISSET reads one bit of the set, below its size.
            .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
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

    #[test]
    fn a_list_is_read_as_the_kernel_writes_it() {
        assert_eq!(parse_list("0-3,6,8-9\n"), Ok(vec![0, 1, 2, 3, 6, 8, 9]));
        assert_eq!(parse_list("7"), Ok(vec![7]));
        assert_eq!(parse_list(""), Ok(vec![]));
        assert_eq!(parse_list("4-7,5"), Ok(vec![4, 5, 6, 7]));
    }

    #[test]
    fn a_list_that_is_not_one_is_refused() {
        for text in [
            "3-1",
            "a",
            "1,,2",
            "-1",
            "1-",
            "4096",
            "99999999999999999999",
            "1 2",
            "0x3",
            "+1",
            ",",
        ] {
            assert!(parse_list(text).is_err(), "{text:?} was taken");
        }
    }

    #[test]
    fn the_big_cores_of_a_big_little_board_are_taken() {
        let rk3588 = |cpu: usize| Some(if cpu < 4 { 405 } else { 1024 });
        assert_eq!(fastest(&[0, 1, 2, 3, 4, 5, 6, 7], rk3588), vec![4, 5, 6, 7]);
    }

    #[test]
    fn only_allowed_cores_are_taken_and_the_fastest_of_those() {
        let rk3588 = |cpu: usize| Some(if cpu < 4 { 405 } else { 1024 });
        assert_eq!(fastest(&[0, 1, 5], rk3588), vec![5]);
        // Little cores alone are all alike: a machine on them never changes kind.
        assert_eq!(fastest(&[0, 1, 2], rk3588), vec![0, 1, 2]);
    }

    #[test]
    fn a_host_that_says_nothing_of_capacity_gives_every_allowed_core() {
        assert_eq!(fastest(&[3, 1, 2], |_| None), vec![1, 2, 3]);
        assert_eq!(fastest(&[0, 1], |_| Some(1024)), vec![0, 1]);
    }

    #[test]
    fn a_choice_must_be_allowed_and_never_empty() {
        let none = |_| None;
        assert_eq!(host_cores(Some(&[6, 4]), &[0, 4, 6], none), Ok(vec![4, 6]));
        let refused = host_cores(Some(&[4, 9]), &[0, 4], none).unwrap_err();
        assert!(refused.contains("CPU 9"), "{refused}");
        assert!(host_cores(Some(&[]), &[0], none).is_err());
        assert!(host_cores(None, &[], none).is_err());
    }

    proptest! {
        #[test]
        fn a_written_list_reads_back_the_same(cpus in proptest::collection::vec(0usize..=MAX_CPU, 0..40)) {
            let mut expected = cpus.clone();
            expected.sort_unstable();
            expected.dedup();
            prop_assert_eq!(parse_list(&List(&cpus).to_string()), Ok(expected));
        }

        #[test]
        fn the_fastest_are_allowed_alike_and_none_allowed_is_faster(
            capacities in proptest::collection::vec(proptest::option::of(1u32..2048), 1..16),
            allowed in proptest::collection::btree_set(0usize..16, 1..16),
        ) {
            let allowed: Vec<usize> = allowed.into_iter().collect();
            let capacity = |cpu: usize| capacities.get(cpu).copied().flatten();
            let cores = fastest(&allowed, capacity);
            prop_assert!(!cores.is_empty());
            prop_assert!(cores.iter().all(|cpu| allowed.contains(cpu)));
            let speeds: std::collections::BTreeSet<Option<u32>> = cores.iter().map(|&cpu| capacity(cpu)).collect();
            prop_assert_eq!(speeds.len(), 1, "one kind of core");
            let top = allowed.iter().filter_map(|&cpu| capacity(cpu)).max();
            prop_assert_eq!(speeds.into_iter().next().unwrap(), top);
        }
    }
}
