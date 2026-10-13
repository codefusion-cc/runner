//! Folders that stand in for `/proc` and `/sys`, with the files of an RK3588 board (8 GB, 4 fast and 4 slow
//! cores, a USB-C supply and no battery) as they were read from one.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub const MEMINFO: &str = "\
MemTotal:        8129820 kB
MemFree:         6265048 kB
MemAvailable:    7712136 kB
Buffers:          114020 kB
Cached:          1237304 kB
SwapCached:            0 kB
SwapTotal:       4064908 kB
SwapFree:        4064908 kB
Dirty:             41028 kB
HugePages_Total:       0
Hugepagesize:       2048 kB
";

pub const VMSTAT: &str = "nr_free_pages 1566262\npswpin 0\npswpout 0\npgpgin 482811\n";

pub const PSI: &str = "\
some avg10=0.00 avg60=0.00 avg300=0.00 total=0
full avg10=0.00 avg60=0.00 avg300=0.00 total=0
";

pub const STAT: &str = "\
cpu  3959 558 9313 3345824 2887 0 4452 0 0 0
cpu0 318 110 1224 419516 118 0 143 0 0 0
intr 1234 0 0 0
ctxt 2271120
btime 1791890324
processes 5592
";

/// A process the board ran, started 4878.23 s after the board did.
pub const PID: u32 = 5591;
pub const PROGRAM: &str = "/opt/codefusion-runner/bin/codefusion-runner";

pub fn stat_of(pid: u32, name: &str, start_ticks: u64) -> String {
    format!(
        "{pid} ({name}) R 5590 5590 5590 0 -1 4194304 79 0 0 0 0 0 0 0 20 0 1 0 {start_ticks} 8069120 195 \
         18446744073709551615 187650376400896 187650376434192 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0\n"
    )
}

/// A folder of fixtures, removed when dropped.
pub struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    /// An empty tree: a host that has none of the files.
    pub fn empty() -> Tree {
        Tree {
            dir: tempfile::tempdir().expect("a temporary folder"),
        }
    }

    /// The board, whole.
    pub fn board() -> Tree {
        let tree = Tree::empty();
        tree.file("/proc/meminfo", MEMINFO);
        tree.file("/proc/vmstat", VMSTAT);
        tree.file("/proc/pressure/memory", PSI);
        tree.file("/proc/stat", STAT);
        tree.file("/proc/loadavg", "0.13 0.09 0.03 1/265 4955\n");
        tree.file(
            "/proc/sys/kernel/random/boot_id",
            "575472c0-34c1-450a-8d2a-4b90fc2982ed\n",
        );
        tree.file("/proc/sys/kernel/hostname", "board\n");
        tree.bytes("/proc/device-tree/model", b"Generic RK3588 Board\0");
        tree.file("/sys/devices/system/cpu/online", "0-7\n");
        for cpu in 0..8 {
            let capacity = if cpu < 4 { "405\n" } else { "1024\n" };
            tree.file(
                &format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity"),
                capacity,
            );
        }
        tree.zone(
            0,
            "soc-thermal",
            44384,
            &[("passive", 60000), ("passive", 85000), ("critical", 115000)],
        );
        for (index, name) in ["bigcore0", "bigcore1", "littlecore", "center", "gpu", "npu"]
            .into_iter()
            .enumerate()
        {
            tree.zone(
                index + 1,
                &format!("{name}-thermal"),
                44384,
                &[("critical", 115000)],
            );
        }
        let supply = "/sys/class/power_supply/tcpm-source-psy-4-0022";
        tree.file(&format!("{supply}/type"), "USB\n");
        tree.file(&format!("{supply}/online"), "1\n");
        tree.process(PID, "cat", 487_823, PROGRAM);
        tree
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn path(&self, path: &str) -> PathBuf {
        self.root().join(path.trim_start_matches('/'))
    }

    pub fn bytes(&self, path: &str, contents: &[u8]) {
        let path = self.path(path);
        std::fs::create_dir_all(path.parent().expect("a folder")).expect("folders");
        std::fs::write(path, contents).expect("a file");
    }

    pub fn file(&self, path: &str, contents: &str) {
        self.bytes(path, contents.as_bytes());
    }

    pub fn remove(&self, path: &str) {
        let path = self.path(path);
        if path.is_dir() {
            std::fs::remove_dir_all(path).expect("removed");
        } else {
            std::fs::remove_file(path).expect("removed");
        }
    }

    /// A thermal zone with trip points of (type, millidegrees).
    pub fn zone(&self, index: usize, name: &str, millidegrees: i32, trips: &[(&str, i32)]) {
        let zone = format!("/sys/class/thermal/thermal_zone{index}");
        self.file(&format!("{zone}/type"), &format!("{name}\n"));
        self.file(&format!("{zone}/mode"), "enabled\n");
        self.file(&format!("{zone}/temp"), &format!("{millidegrees}\n"));
        for (trip, (kind, at)) in trips.iter().enumerate() {
            self.file(&format!("{zone}/trip_point_{trip}_type"), &format!("{kind}\n"));
            self.file(&format!("{zone}/trip_point_{trip}_temp"), &format!("{at}\n"));
            self.file(&format!("{zone}/trip_point_{trip}_hyst"), "2000\n");
        }
    }

    /// A power supply with these files.
    pub fn supply(&self, name: &str, files: &[(&str, &str)]) {
        for (file, contents) in files {
            self.file(
                &format!("/sys/class/power_supply/{name}/{file}"),
                &format!("{contents}\n"),
            );
        }
    }

    /// A process: its stat and the link to its program.
    pub fn process(&self, pid: u32, name: &str, start_ticks: u64, program: &str) {
        self.file(&format!("/proc/{pid}/stat"), &stat_of(pid, name, start_ticks));
        let link = self.path(&format!("/proc/{pid}/exe"));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(program, link).expect("a link");
    }
}
