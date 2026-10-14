//! The text of files in `/proc` and `/sys`, read without I/O. What the kernel writes is trusted as far as its
//! format goes and no further: a value that cannot be right is an error or `None`, never a guess.

use cf_budget::HostError;

/// The longest file read. `/proc/stat` is the largest: its interrupt line grows with the host, to tens of kB on a
/// large one. A file longer than this is not one the kernel wrote, and is not read in part.
pub const MAX_FILE_BYTES: usize = 1 << 20;

/// The most CPUs a list may name. Linux builds for at most 8192 today.
pub(crate) const MAX_CPUS: usize = 1 << 16;

/// What is shown of a value that cannot be right.
fn shown(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(64) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_string(),
    }
}

pub(crate) fn implausible(what: &'static str, value: &str) -> HostError {
    HostError::Implausible {
        what,
        value: shown(value),
    }
}

/// Digits only, as a number; `None` for anything else, an empty text and a number past `u64`.
fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// What `/proc/meminfo` says of memory and swap, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meminfo {
    /// `MemTotal`: the memory the kernel manages, a little less than what is installed.
    pub total_bytes: u64,
    /// `MemAvailable`: what can be handed out without swapping, the kernel's own estimate of free memory, the page
    /// cache it can drop and the reclaimable slab. Not `MemFree`, which leaves the cache out.
    pub available_bytes: u64,
    /// `SwapTotal`.
    pub swap_total_bytes: u64,
    /// `SwapFree`.
    pub swap_free_bytes: u64,
}

/// `/proc/meminfo`. Each of the four lines read must be there once, as a number of kB.
pub fn meminfo(text: &str) -> Result<Meminfo, HostError> {
    let field = |name: &'static str| {
        let mut found = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if key != name {
                continue;
            }
            let bytes = value
                .trim()
                .strip_suffix(" kB")
                .and_then(|kib| number(kib.trim_end()))
                .and_then(|kib| kib.checked_mul(1024))
                .ok_or_else(|| implausible(name, value))?;
            if found.replace(bytes).is_some() {
                return Err(implausible(name, "named twice"));
            }
        }
        found.ok_or_else(|| implausible(name, "missing"))
    };
    let memory = Meminfo {
        total_bytes: field("MemTotal")?,
        available_bytes: field("MemAvailable")?,
        swap_total_bytes: field("SwapTotal")?,
        swap_free_bytes: field("SwapFree")?,
    };
    if memory.total_bytes == 0 {
        return Err(implausible("MemTotal", "0 kB"));
    }
    Ok(memory)
}

/// A file that holds one number, such as `cpu_capacity`.
pub fn count(text: &str) -> Option<u64> {
    number(text.trim())
}

/// A core's design from its `midr_el1` (`0x00000000414fd0b0`): the implementer and the part number, without the
/// variant and revision, which a core of the same design may differ in. `None` for anything but a hexadecimal number.
pub(crate) fn midr_design(text: &str) -> Option<u64> {
    let digits = text.trim().strip_prefix("0x")?;
    if digits.is_empty() || digits.len() > 16 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    // Implementer: bits 31 to 24; part number: bits 15 to 4.
    Some(u64::from_str_radix(digits, 16).ok()? & 0xff00_fff0)
}

/// One counter of a file of `name value` lines: `pswpout` of `/proc/vmstat`, `oom_kill` of a cgroup's
/// `memory.events`. `None` when it is not there once as a number.
pub fn counter(text: &str, name: &str) -> Option<u64> {
    let mut values = text.lines().filter_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (key == name).then_some(value)
    });
    let value = number(values.next()?.trim())?;
    values.next().is_none().then_some(value)
}

/// A swap area of `/proc/swaps`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Swap {
    /// A zram device (`/dev/zramN`): compressed memory, not a disk.
    pub zram: bool,
    /// What is in use there, in bytes.
    pub used_bytes: u64,
}

/// `/proc/swaps`: a header, then `Filename Type Size Used Priority` for each swap area, sizes in KiB. `None` when
/// the header or a line is not that.
pub fn swaps(text: &str) -> Option<Vec<Swap>> {
    let mut lines = text.lines();
    if !lines
        .next()?
        .split_whitespace()
        .eq(["Filename", "Type", "Size", "Used", "Priority"])
    {
        return None;
    }
    lines
        .map(|line| {
            let [name, kind, size, used, priority] = line.split_whitespace().collect::<Vec<_>>()[..] else {
                return None;
            };
            let (size, used) = (number(size)?, number(used)?);
            priority.parse::<i32>().ok()?;
            if used > size || !["partition", "file"].contains(&kind) {
                return None;
            }
            let zram = kind == "partition"
                && name.strip_prefix("/dev/zram").is_some_and(|index| {
                    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
                });
            Some(Swap {
                zram,
                used_bytes: used.checked_mul(1024)?,
            })
        })
        .collect()
}

/// A share of time over the last 10 s from a pressure file (PSI), in hundredths of a percent: 1234 is 12.34%.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Psi {
    /// `some avg10`: at least one task was stalled on the resource.
    pub some: u32,
    /// `full avg10`: every task that wanted to run was stalled on it at once.
    pub full: u32,
}

/// `12.34` as 1234; at most 100.00.
fn hundredths(text: &str) -> Option<u32> {
    let (whole, part) = text.split_once('.').unwrap_or((text, "0"));
    if whole.len() > 3 || part.is_empty() || part.len() > 2 {
        return None;
    }
    let part = number(part)? * if part.len() == 1 { 10 } else { 1 };
    let value = number(whole)? * 100 + part;
    u32::try_from(value).ok().filter(|value| *value <= 10_000)
}

/// A pressure file, such as `/proc/pressure/memory`: the `avg10` of its `some` and `full` lines.
pub fn psi(text: &str) -> Result<Psi, HostError> {
    let avg10 = |kind: &'static str| {
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix(kind)?.strip_prefix(' '))
            .ok_or_else(|| implausible(kind, "missing"))?;
        line.split_whitespace()
            .find_map(|pair| pair.strip_prefix("avg10="))
            .and_then(hundredths)
            .ok_or_else(|| implausible(kind, line))
    };
    Ok(Psi {
        some: avg10("some")?,
        full: avg10("full")?,
    })
}

/// A list of CPUs as the kernel writes it, such as `0-3,6,8-9`, in order with none twice; `None` for anything else
/// and for more than [`MAX_CPUS`].
pub fn cpu_list(text: &str) -> Option<Vec<u32>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(Vec::new());
    }
    let mut cpus: Vec<u32> = Vec::new();
    for part in text.split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let first = u32::try_from(number(first)?).ok()?;
        let last = u32::try_from(number(last)?).ok()?;
        if first > last || cpus.last().is_some_and(|before| *before >= first) {
            return None;
        }
        if cpus.len() + (last - first) as usize >= MAX_CPUS {
            return None;
        }
        cpus.extend(first..=last);
    }
    Some(cpus)
}

/// When the host started, from `/proc/stat`'s `btime`: seconds since 1970.
pub fn btime(stat: &str) -> Option<i64> {
    let value = stat.lines().find_map(|line| line.strip_prefix("btime "))?;
    i64::try_from(number(value.trim())?).ok().filter(|secs| *secs > 0)
}

/// The time all CPUs spent since the host started, in ticks, from `/proc/stat`'s first line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTicks {
    /// User, nice, system, interrupts and the time the hypervisor took (steal). The guests' time is in user's
    /// and nice's already.
    pub busy: u64,
    /// Idle, and waiting for I/O with nothing else to do.
    pub idle: u64,
}

/// `/proc/stat`'s `cpu` line: user, nice, system, idle, iowait, irq, softirq, steal, guest, guest_nice. A kernel
/// older than one of them writes fewer, which count as zero; the first four must be there.
pub fn cpu_ticks(stat: &str) -> Option<CpuTicks> {
    let line = stat.lines().find_map(|line| line.strip_prefix("cpu "))?;
    let ticks = line
        .split_whitespace()
        .map(number)
        .collect::<Option<Vec<u64>>>()?;
    if ticks.len() < 4 {
        return None;
    }
    let at = |index: usize| ticks.get(index).copied().unwrap_or(0);
    let sum = |indices: &[usize]| {
        indices
            .iter()
            .try_fold(0u64, |sum, index| sum.checked_add(at(*index)))
    };
    Some(CpuTicks {
        busy: sum(&[0, 1, 2, 5, 6, 7])?,
        idle: sum(&[3, 4])?,
    })
}

/// When a process started, in clock ticks after the host did: field 22 of `/proc/<pid>/stat`. The name in
/// field 2 is the program's to choose, spaces and parentheses included, so the fields are counted after its last
/// `)`.
pub fn start_ticks(stat: &str) -> Option<u64> {
    let (head, rest) = stat.rsplit_once(')')?;
    let (pid, _name) = head.split_once(" (")?;
    number(pid)?;
    // Field 3 is the first after the name.
    number(rest.split_whitespace().nth(22 - 3)?)
}

/// `/proc/loadavg`: the load averages of 1, 5 and 15 minutes.
pub fn loadavg(text: &str) -> Option<[f64; 3]> {
    let mut values = text.split_whitespace().map(|value| {
        let value: f64 = value.parse().ok()?;
        (value.is_finite() && value >= 0.0).then_some(value)
    });
    Some([values.next()??, values.next()??, values.next()??])
}

/// `/proc/sys/kernel/random/boot_id`: a UUID, lower case, whole.
pub fn boot_id(text: &str) -> Option<String> {
    let id = text.trim();
    let well_formed = id.len() == 36
        && id.char_indices().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        });
    well_formed.then(|| id.to_string())
}

/// A temperature in thousandths of a degree Celsius, as `/sys/class/thermal` writes it. `None` past what any
/// sensor reads: the kernel's mark for an invalid temperature is -274 degrees.
pub fn millidegrees(text: &str) -> Option<i32> {
    let value: i32 = text.trim().parse().ok()?;
    (-100_000..=300_000).contains(&value).then_some(value)
}

/// The most of a name kept.
const MAX_NAME_CHARS: usize = 128;

/// A name the kernel or the firmware wrote: up to its first NUL, trimmed, at most 128 characters, and `None` when
/// empty, not UTF-8 or holding a control character.
pub fn text(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    let text = std::str::from_utf8(&bytes[..end]).ok()?.trim();
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.chars().take(MAX_NAME_CHARS).collect())
}

/// What firmware writes into DMI when nobody filled it in.
const PLACEHOLDERS: [&str; 9] = [
    "to be filled by o.e.m.",
    "default string",
    "system manufacturer",
    "system product name",
    "not applicable",
    "not specified",
    "o.e.m.",
    "none",
    "unknown",
];

/// A host's model from DMI (`sys_vendor` and `product_name`), such as `Lenovo ThinkCentre M75q`: the vendor once,
/// and what firmware leaves as a placeholder left out. `None` with neither.
pub(crate) fn dmi_model(vendor: Option<&str>, product: Option<&str>) -> Option<String> {
    fn said(text: Option<&str>) -> Option<&str> {
        text.map(str::trim)
            .filter(|text| !text.is_empty() && !PLACEHOLDERS.contains(&text.to_lowercase().as_str()))
    }
    match (said(vendor), said(product)) {
        (Some(vendor), Some(product)) if product.to_lowercase().starts_with(&vendor.to_lowercase()) => {
            Some(product.to_string())
        }
        (Some(vendor), Some(product)) => Some(format!("{vendor} {product}")),
        (one, other) => one.or(other).map(str::to_string),
    }
}
