//! Many callers at once: the budget behind one mutex, as the daemon holds it.

mod common;

use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use cf_budget::{Budget, GIB, Refusal, Size};
use common::*;

/// An open budget with memory for exactly `slots` VMs of 4 GB, and CPUs and cap for more.
fn room_for(slots: u64) -> Budget {
    let mut budget = open(one_vcpu(dedicated()));
    let mut reading = calm(secs(61));
    reading.available_bytes = slots * (4 * GIB + cf_budget::Settings::VM_HOST_OVERHEAD)
        + 2 * GIB
        + (4 * GIB + cf_budget::Settings::VM_HOST_OVERHEAD - 1);
    budget.observe(secs(61), Some(reading));
    budget
}

#[test]
fn forty_claims_for_two_slots() {
    for _ in 0..50 {
        let budget = Arc::new(Mutex::new(room_for(2)));
        let start = Arc::new(Barrier::new(40));
        let claims: Vec<_> = (0..40)
            .map(|_| {
                let (budget, start) = (budget.clone(), start.clone());
                thread::spawn(move || {
                    start.wait();
                    budget.lock().unwrap().claim(secs(61), Size::G4)
                })
            })
            .collect();
        let answers: Vec<_> = claims.into_iter().map(|claim| claim.join().unwrap()).collect();
        let mut granted: Vec<_> = answers
            .iter()
            .filter_map(|answer| answer.ok())
            .map(|grant| grant.slot)
            .collect();
        granted.sort();
        granted.dedup();
        assert_eq!(granted.len(), 2, "two slots, each its own");
        let refused = answers
            .iter()
            .filter(|answer| matches!(answer, Err(Refusal::Memory { .. })))
            .count();
        assert_eq!(refused, 38);
        let budget = budget.lock().unwrap();
        assert_eq!((budget.committed_bytes(), budget.slots()), (8 * GIB, 2));
    }
}

#[test]
fn a_claim_racing_a_release() {
    // Two slots, both taken. One caller gives one back while another asks for one. Whoever is first, the Mac
    // ends with two slots promised or with one, and never with three.
    let mut both = [0, 0];
    for _ in 0..400 {
        let mut full = room_for(2);
        let held = full.claim(secs(61), Size::G4).unwrap().slot;
        full.claim(secs(61), Size::G4).unwrap();
        let budget = Arc::new(Mutex::new(full));
        let start = Arc::new(Barrier::new(2));
        let releasing = {
            let (budget, start) = (budget.clone(), start.clone());
            thread::spawn(move || {
                start.wait();
                budget.lock().unwrap().release(held)
            })
        };
        let claiming = {
            let (budget, start) = (budget.clone(), start.clone());
            thread::spawn(move || {
                start.wait();
                budget.lock().unwrap().claim(secs(61), Size::G4)
            })
        };
        assert_eq!(releasing.join().unwrap().unwrap().vm_bytes, 4 * GIB);
        let claimed = claiming.join().unwrap();
        let budget = budget.lock().unwrap();
        match claimed {
            Ok(grant) => {
                assert_ne!(grant.slot, held);
                assert_eq!((budget.committed_bytes(), budget.slots()), (8 * GIB, 2));
                both[0] += 1;
            }
            Err(refusal) => {
                assert!(matches!(refusal, Refusal::Memory { .. }), "{refusal:?}");
                assert_eq!((budget.committed_bytes(), budget.slots()), (4 * GIB, 1));
                both[1] += 1;
            }
        }
    }
    assert_eq!(both[0] + both[1], 400);
}

#[test]
fn claims_releases_and_readings_from_many_threads_never_overfill() {
    let budget = Arc::new(Mutex::new(room_for(2)));
    let start = Arc::new(Barrier::new(9));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (budget, start) = (budget.clone(), start.clone());
            thread::spawn(move || {
                start.wait();
                let mut most = 0;
                for _ in 0..2000 {
                    let granted = budget.lock().unwrap().claim(secs(61), Size::G4);
                    most = most.max(budget.lock().unwrap().committed_bytes());
                    if let Ok(grant) = granted {
                        budget.lock().unwrap().release(grant.slot).unwrap();
                    }
                }
                most
            })
        })
        .collect();
    // Meanwhile the reader keeps handing in the same reading for the same second.
    let reader = {
        let (budget, start) = (budget.clone(), start.clone());
        thread::spawn(move || {
            start.wait();
            let reading = budget.lock().unwrap().status(secs(61)).reading.unwrap();
            for _ in 0..2000 {
                budget.lock().unwrap().observe(secs(61), Some(reading));
            }
        })
    };
    let most = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .max()
        .unwrap();
    reader.join().unwrap();
    assert!(most <= 8 * GIB, "{most} bytes promised at once with room for two");
    assert_eq!(budget.lock().unwrap().slots(), 0);
}
