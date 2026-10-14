//! The scripted host, driven the way the daemon drives a real one: one reading a second into a real budget.

use cf_budget::FakeHost;
use cf_budget::{
    Budget, GIB, Host, HostError, HostFacts, Level, Millis, Mode, Now, Pressure, Refusal, Settings, Size,
    Trouble,
};

const FACTS: HostFacts = HostFacts {
    total_bytes: 16 * GIB,
    cores: 8,
};

/// One second passes and the budget gets what the host says.
fn tick(host: &mut FakeHost, budget: &mut Budget) -> cf_budget::Observed {
    host.pass(Millis::secs(1));
    let reading = host.read().ok();
    budget.observe(host.now(), reading)
}

fn ticks(host: &mut FakeHost, budget: &mut Budget, seconds: u32) {
    for _ in 0..seconds {
        tick(host, budget);
    }
}

fn start(mode: Mode) -> (FakeHost, Budget) {
    let host = FakeHost::new(FACTS);
    let budget = Budget::new(Settings::for_host(host.facts().unwrap(), mode)).unwrap();
    (host, budget)
}

#[test]
fn a_calm_dedicated_mac_opens_after_a_minute_and_not_before() {
    let (mut host, mut budget) = start(Mode::Dedicated);
    ticks(&mut host, &mut budget, 60);
    assert!(matches!(
        budget.claim(host.now(), Size::G4),
        Err(Refusal::Calming { .. })
    ));
    tick(&mut host, &mut budget);
    assert_eq!(budget.level(host.now()), Level::Open);
    assert!(budget.claim(host.now(), Size::G4).is_ok());
    assert_eq!(host.reads(), 61);
}

#[test]
fn a_reading_carries_the_hosts_own_time() {
    let mut host = FakeHost::new(FACTS);
    host.pass(Millis::secs(3));
    host.sleep(Millis::secs(100));
    let now = host.now();
    assert_eq!(now.elapsed.since(now.awake), Millis::secs(100));
    assert_eq!(host.read().unwrap().at, now);
    // A test cannot hand out a reading from another moment by accident.
    host.set(|reading| reading.at = Now::default());
    assert_eq!(host.read().unwrap().at, now);
}

#[test]
fn reads_that_fail_close_the_mac_until_a_minute_after_they_work_again() {
    let (mut host, mut budget) = start(Mode::Dedicated);
    ticks(&mut host, &mut budget, 61);
    let error = HostError::Call {
        what: "host_statistics64",
        code: 5,
    };
    host.fail(Some(error.clone()));
    assert_eq!(host.read(), Err(error));
    tick(&mut host, &mut budget);
    assert_eq!(budget.claim(host.now(), Size::G2), Err(Refusal::Blind));
    host.fail(None);
    ticks(&mut host, &mut budget, 60);
    assert!(matches!(
        budget.claim(host.now(), Size::G2),
        Err(Refusal::Calming { .. })
    ));
    tick(&mut host, &mut budget);
    assert!(budget.claim(host.now(), Size::G2).is_ok());
}

#[test]
fn a_workstation_opens_only_after_fifteen_minutes_without_its_person() {
    let (mut host, mut budget) = start(Mode::Workstation);
    ticks(&mut host, &mut budget, 15 * 60 - 1);
    assert_eq!(budget.claim(host.now(), Size::G4), Err(Refusal::PersonThere));
    tick(&mut host, &mut budget);
    let grant = budget.claim(host.now(), Size::G4).unwrap();
    budget.confirm(grant.slot).unwrap();
    // The person comes back: nothing new opens, and the job that runs stays.
    host.touch();
    let observed = tick(&mut host, &mut budget);
    assert_eq!(budget.claim(host.now(), Size::G2), Err(Refusal::PersonThere));
    assert_eq!((observed.evict, budget.slots()), (None, 1));
}

#[test]
fn a_sleeping_mac_wakes_up_closed_and_says_how_long_it_slept() {
    let (mut host, mut budget) = start(Mode::Dedicated);
    ticks(&mut host, &mut budget, 61);
    host.sleep(Millis::mins(40));
    let observed = tick(&mut host, &mut budget);
    assert_eq!(observed.slept, Millis::mins(40));
    assert!(matches!(
        budget.claim(host.now(), Size::G2),
        Err(Refusal::Calming { .. })
    ));
}

#[test]
fn a_clock_set_back_closes_the_mac() {
    let (mut host, mut budget) = start(Mode::Dedicated);
    ticks(&mut host, &mut budget, 61);
    host.set_now(Now {
        awake: Millis::secs(10),
        elapsed: Millis::secs(10),
    });
    let reading = host.read().ok();
    budget.observe(host.now(), reading);
    assert!(matches!(
        budget.claim(host.now(), Size::G2),
        Err(Refusal::Calming { .. })
    ));
}

#[test]
fn what_a_test_sets_is_what_the_budget_sees() {
    let (mut host, mut budget) = start(Mode::Dedicated);
    ticks(&mut host, &mut budget, 61);
    host.set(|reading| reading.pressure = Pressure::Warning);
    tick(&mut host, &mut budget);
    assert_eq!(
        budget.claim(host.now(), Size::G2),
        Err(Refusal::Trouble(Trouble::Pressure))
    );
    host.set(|reading| {
        reading.pressure = Pressure::Normal;
        reading.on_battery = true;
    });
    tick(&mut host, &mut budget);
    assert_eq!(
        budget.claim(host.now(), Size::G2),
        Err(Refusal::Trouble(Trouble::Battery))
    );
}

#[test]
fn facts_that_cannot_be_read_are_an_error_for_the_caller() {
    let mut host = FakeHost::new(FACTS);
    assert_eq!(host.facts(), Ok(FACTS));
    let error = HostError::Implausible {
        what: "hw.memsize",
        value: "0".into(),
    };
    host.set_facts(Err(error.clone()));
    assert_eq!(host.facts(), Err(error));
}
