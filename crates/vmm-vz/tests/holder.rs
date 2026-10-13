//! Finding the process that holds a machine's files, against real processes of this Mac: a program that holds a
//! file open stands in for the framework's machine process, which only a real machine starts.
#![cfg(target_os = "macos")]

use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use cf_vmm_vz::Holder;

const SLEEP: &str = "/bin/sleep";

/// A process of `/bin/sleep` that holds a file open as its standard input, until it is dropped, even by a test that
/// failed.
struct Sleeper(Child);

impl Sleeper {
    fn pid(&self) -> Option<i32> {
        Some(self.0.id() as i32)
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn holding(file: &Path) -> Sleeper {
    Sleeper(
        Command::new(SLEEP)
            .arg("60")
            .stdin(Stdio::from(File::open(file).expect("the file opens")))
            .spawn()
            .expect("sleep starts"),
    )
}

fn files() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a folder");
    let (held, other) = (dir.path().join("console.log"), dir.path().join("scratch.img"));
    std::fs::write(&held, b"x").expect("a file");
    std::fs::write(&other, b"y").expect("a file");
    (dir, held, other)
}

#[test]
fn the_process_that_holds_a_machines_file_is_found_and_what_it_holds_read() {
    let (_dir, held, other) = files();
    let child = holding(&held);
    let holder = Holder::of(Path::new(SLEEP), &[&other, &held]);
    assert_eq!(holder.pid(), child.pid());
    let resident = holder.resident().expect("what it holds");
    // A program as small as sleep still holds some pages, and nowhere near a gigabyte.
    assert!((1..1 << 30).contains(&resident), "{resident}");
    drop(child);
    assert_eq!(holder.pid(), None, "a process that is gone is not read");
    assert_eq!(holder.resident(), None);
    // A machine booted again runs in a new process, which holds the same files.
    let again = holding(&held);
    assert_eq!(holder.pid(), again.pid());
}

#[test]
fn a_process_of_another_program_or_files_nobody_holds_give_nothing() {
    let (_dir, held, other) = files();
    let _child = holding(&held);
    // The framework's machine process is not sleep: the file is held, but not by a machine.
    assert_eq!(Holder::machine(&[&held]).resident(), None);
    assert_eq!(Holder::of(Path::new(SLEEP), &[&other]).resident(), None);
    assert_eq!(Holder::of(Path::new(SLEEP), &[]).resident(), None);
    // A file that is not there names nothing.
    let gone = held.with_extension("gone");
    assert_eq!(Holder::of(Path::new(SLEEP), &[&gone]).resident(), None);
}

#[test]
fn the_same_file_under_another_path_is_the_same_file() {
    let (dir, held, _) = files();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&held, &link).expect("a link");
    let child = holding(&link);
    assert_eq!(Holder::of(Path::new(SLEEP), &[&held]).pid(), child.pid());
}
