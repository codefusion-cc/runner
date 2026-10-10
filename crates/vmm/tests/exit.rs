//! A machine's end as its monitor keeps it: once, whoever says it first.

use cf_vmm::{ExitCell, VmExit};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn a_machine_ends_once_and_the_first_word_stays() {
    let cell = ExitCell::default();
    assert_eq!(cell.wait(Duration::ZERO), None);
    cell.end(VmExit::Error("the monitor lost it".into()));
    cell.end(VmExit::Guest);
    cell.found_stopped();
    cell.stop_asked();
    cell.found_stopped();
    for _ in 0..2 {
        assert_eq!(
            cell.wait(Duration::from_secs(5)),
            Some(VmExit::Error("the monitor lost it".into()))
        );
    }
}

#[test]
fn a_machine_found_stopped_after_the_host_asked_was_stopped_by_the_host() {
    // The host's stop and a look at the machine race: the look may come first.
    let cell = ExitCell::default();
    cell.stop_asked();
    cell.found_stopped();
    cell.end(VmExit::Guest);
    assert_eq!(cell.wait(Duration::ZERO), Some(VmExit::Stopped));

    // Nobody asked: the guest powered off.
    let cell = ExitCell::default();
    cell.found_stopped();
    assert_eq!(cell.wait(Duration::ZERO), Some(VmExit::Guest));
}

#[test]
fn a_wait_ends_with_its_time_or_with_the_machine_whichever_is_first() {
    let cell = Arc::new(ExitCell::default());
    let started = Instant::now();
    assert_eq!(cell.wait(Duration::from_millis(60)), None);
    assert!(started.elapsed() >= Duration::from_millis(60));

    let ender = {
        let cell = cell.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cell.end(VmExit::Stopped);
        })
    };
    let started = Instant::now();
    // The longest wait there is neither overflows a clock nor returns before the end.
    assert_eq!(cell.wait(Duration::MAX), Some(VmExit::Stopped));
    assert!(started.elapsed() < Duration::from_secs(5));
    ender.join().unwrap();
}
