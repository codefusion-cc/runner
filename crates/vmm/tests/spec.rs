//! What a specification must refuse before a hypervisor sees it, and how the scripted monitor behaves.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use cf_vmm::testing::{FakeVmm, Script, Stop};
use cf_vmm::{Disk, MIN_MEMORY, Network, SpecError, VmExit, VmSpec, Vmm, VmmError};

fn spec() -> VmSpec {
    VmSpec {
        kernel: PathBuf::from("Image"),
        initramfs: PathBuf::from("initramfs"),
        memory_bytes: 1 << 30,
        cpus: 2,
        disks: vec![disk("cf-image", true), disk("cf-scratch", false)],
        network: Network::Nat,
        console: PathBuf::from("console.log"),
    }
}

fn disk(id: &str, read_only: bool) -> Disk {
    Disk {
        path: PathBuf::from(format!("{id}.img")),
        id: id.into(),
        read_only,
    }
}

#[test]
fn memory_is_whole_mebibytes_from_the_minimum_up() {
    for (bytes, fine) in [
        (0, false),
        (MIN_MEMORY - (1 << 20), false),
        (MIN_MEMORY, true),
        (MIN_MEMORY + 1, false),
        (MIN_MEMORY + (1 << 20), true),
        (8 << 30, true),
        (u64::MAX, false),
    ] {
        let result = VmSpec {
            memory_bytes: bytes,
            ..spec()
        }
        .validate();
        assert_eq!(result.is_ok(), fine, "{bytes}: {result:?}");
        if !fine {
            assert_eq!(result, Err(SpecError::Memory(bytes)));
        }
    }
}

#[test]
fn a_machine_has_one_to_sixty_four_cpus() {
    for (cpus, fine) in [
        (0, false),
        (1, true),
        (64, true),
        (65, false),
        (usize::MAX, false),
    ] {
        assert_eq!(VmSpec { cpus, ..spec() }.validate().is_ok(), fine, "{cpus}");
    }
}

#[test]
fn a_disk_id_is_what_the_guest_can_carry_as_a_serial() {
    for (id, fine) in [
        ("", false),
        ("a", true),
        ("cf-scratch", true),
        ("abcdefghij0123456789", true),
        ("abcdefghij0123456789x", false),
        ("Cache", false),
        ("a b", false),
        ("../x", false),
        ("naïve", false),
    ] {
        let result = VmSpec {
            disks: vec![disk(id, true)],
            ..spec()
        }
        .validate();
        assert_eq!(result.is_ok(), fine, "{id:?}");
    }
}

#[test]
fn two_disks_with_one_id_are_refused() {
    let result = VmSpec {
        disks: vec![disk("cf-image", true), disk("cf-image", false)],
        ..spec()
    }
    .validate();
    assert_eq!(result, Err(SpecError::DuplicateDisk("cf-image".into())));
}

#[test]
fn the_scripted_monitor_refuses_a_bad_specification_like_a_real_one() {
    let vmm = FakeVmm::new([Script::boot(|_, _| {})]);
    let Err(error) = vmm.start(&VmSpec { cpus: 0, ..spec() }) else {
        panic!("a machine with no CPU started");
    };
    assert!(matches!(error, VmmError::Spec(_)));
    assert!(vmm.started().is_empty());
}

#[test]
fn a_scripted_guest_talks_over_the_channel_and_its_return_is_a_power_off() {
    let vmm = FakeVmm::new([Script::Boot {
        refuse_channel: 2,
        guest: Box::new(|mut channel, _| {
            let mut byte = [0u8; 1];
            channel.read_exact(&mut byte).unwrap();
            channel.write_all(&[byte[0] + 1]).unwrap();
        }),
        stop: Stop::Obeys,
    }]);
    let vm = vmm.start(&spec()).unwrap();
    assert!(vm.channel(6000).is_err());
    assert!(vm.channel(6000).is_err());
    assert_eq!(vm.wait(Duration::from_millis(10)), None);
    let mut channel = vm.channel(6000).unwrap();
    channel.write_all(&[41]).unwrap();
    let mut answer = [0u8; 1];
    channel.read_exact(&mut answer).unwrap();
    assert_eq!(answer, [42]);
    assert_eq!(vm.wait(Duration::from_secs(5)), Some(VmExit::Guest));
    // The end is final: a stop afterwards does not change how it ended.
    vm.stop().unwrap();
    assert_eq!(vm.wait(Duration::ZERO), Some(VmExit::Guest));
}

#[test]
fn a_machine_that_ignores_its_stop_keeps_running_until_it_is_killed() {
    let vmm = FakeVmm::new([Script::Boot {
        refuse_channel: 0,
        guest: Box::new(|_, power| power.wait_for_the_end()),
        stop: Stop::Hangs,
    }]);
    let vm = vmm.start(&spec()).unwrap();
    let _channel = vm.channel(6000).unwrap();
    vm.stop().unwrap();
    assert_eq!(vm.wait(Duration::from_millis(50)), None);
    assert_eq!(vmm.stops(), 1);
    vm.kill();
    assert_eq!(vm.wait(Duration::from_secs(5)), Some(VmExit::Stopped));
}

#[test]
fn a_monitor_with_no_script_left_fails_the_start() {
    let vmm = FakeVmm::new([Script::FailToStart(VmmError::Start("no memory".into()))]);
    assert_eq!(
        vmm.start(&spec()).err(),
        Some(VmmError::Start("no memory".into()))
    );
    assert!(matches!(vmm.start(&spec()), Err(VmmError::Start(_))));
    assert_eq!(vmm.started().len(), 2);
}

#[test]
fn a_machine_that_takes_no_stop_says_so_and_runs_until_it_is_let_go() {
    let vmm = FakeVmm::new([Script::Boot {
        refuse_channel: 0,
        guest: Box::new(|_, power| power.wait_for_the_end()),
        stop: Stop::Refuses,
    }]);
    let vm = vmm.start(&spec()).unwrap();
    let _channel = vm.channel(6000).unwrap();
    assert!(matches!(vm.stop(), Err(VmmError::Stop(_))));
    assert_eq!(vm.wait(Duration::from_millis(50)), None);
    assert_eq!(vmm.stops(), 1);
    vm.kill();
    assert_eq!(vm.wait(Duration::from_secs(5)), Some(VmExit::Stopped));
}
