//! The console's `settings` command as the Runner reads it: the console's keys, each a number or `null`.

use cf_daemon::update::SettingsChange;
use proptest::prelude::*;
use serde_json::{Value, json};

fn read(args: Value) -> Result<SettingsChange, String> {
    SettingsChange::from_args(Some(&args))
}

#[test]
fn each_key_the_runner_has_is_read_and_null_puts_it_back() {
    assert_eq!(
        read(json!({"maxJobs": 3, "idleMinutes": 20, "minFreeMemoryPercent": 8, "maxJobHours": 6})),
        Ok(SettingsChange {
            max_jobs: Some(Some(3)),
            idle_minutes: Some(Some(20)),
            min_free_memory_percent: Some(Some(8)),
            deadline_minutes: Some(Some(360)),
        })
    );
    assert_eq!(
        read(
            json!({"maxJobs": null, "idleMinutes": null, "minFreeMemoryPercent": null, "maxJobHours": null})
        ),
        Ok(SettingsChange {
            max_jobs: Some(None),
            idle_minutes: Some(None),
            min_free_memory_percent: Some(None),
            deadline_minutes: Some(None),
        })
    );
    assert_eq!(
        read(json!({"maxJobs": 1})),
        Ok(SettingsChange {
            max_jobs: Some(Some(1)),
            ..SettingsChange::default()
        }),
        "a key left out is left as it is"
    );
}

#[test]
fn whole_numbers_may_come_as_floats_and_nothing_else_is_whole() {
    assert_eq!(read(json!({"maxJobs": 2.0})).unwrap().max_jobs, Some(Some(2)));
    assert_eq!(
        read(json!({"maxJobs": 0})).unwrap().max_jobs,
        Some(Some(0)),
        "the file says 0 is too few"
    );
    for bad in [
        json!(2.5),
        json!(-1),
        json!("2"),
        json!(true),
        json!([2]),
        json!(4_294_967_296_u64),
    ] {
        assert_eq!(
            read(json!({"maxJobs": bad})),
            Err("jobs at once: a whole number".into()),
            "{bad}"
        );
    }
    assert_eq!(
        read(json!({"minFreeMemoryPercent": 8.5})),
        Err("the memory kept free: a whole number".into())
    );
    assert_eq!(
        read(json!({"idleMinutes": -0.5})),
        Err("idle minutes: a whole number".into())
    );
    assert_eq!(
        read(json!({"idleMinutes": 4_294_967_295_u64}))
            .unwrap()
            .idle_minutes,
        Some(Some(u32::MAX))
    );
}

#[test]
fn the_longest_job_is_hours_in_whole_minutes() {
    assert_eq!(
        read(json!({"maxJobHours": 0.5})).unwrap().deadline_minutes,
        Some(Some(30))
    );
    assert_eq!(
        read(json!({"maxJobHours": 72})).unwrap().deadline_minutes,
        Some(Some(4320))
    );
    assert_eq!(
        read(json!({"maxJobHours": 0})).unwrap().deadline_minutes,
        Some(Some(0))
    );
    for bad in [json!(1.0 / 7.0), json!(-1), json!("6"), json!(1e300)] {
        assert_eq!(
            read(json!({"maxJobHours": bad})),
            Err("the longest job: hours in whole minutes".into()),
            "{bad}"
        );
    }
}

#[test]
fn the_cf_runners_managers_settings_are_refused_by_name() {
    assert_eq!(
        read(json!({"maxJobs": 2, "jobMemoryGb": 6})),
        Err("memory per job: each job gets the memory its label names, cf-2g to cf-8g".into())
    );
    assert_eq!(
        read(json!({"jobCpus": 4})),
        Err("CPUs per job: each job gets the CPUs of its machine's size".into())
    );
    assert_eq!(
        read(json!({"maxLoadPerCpu": 1.5})),
        Err("load per CPU: the Runner starts jobs by the memory and CPUs they get, not by load".into())
    );
    assert_eq!(
        read(json!({"color": "blue"})),
        Err("`color` is no setting of the Runner".into())
    );
    let long = "k".repeat(500);
    assert_eq!(
        read(json!({ long: 1 })),
        Err(format!("`{}` is no setting of the Runner", "k".repeat(40)))
    );
}

#[test]
fn a_command_with_no_settings_is_refused() {
    assert_eq!(
        SettingsChange::from_args(None),
        Err("the command names no settings".into())
    );
    for args in [json!(null), json!([]), json!("maxJobs"), json!(3)] {
        assert_eq!(
            read(args.clone()),
            Err("the command names no settings".into()),
            "{args}"
        );
    }
    assert_eq!(read(json!({})), Err("the command changes no setting".into()));
}

proptest! {
    /// Any change the console's dialog can send is read as it was meant, whatever it holds.
    #[test]
    fn what_the_console_sends_is_read_as_sent(
        jobs in prop::option::of(prop::option::of(1u32..=32)),
        idle in prop::option::of(prop::option::of(0u32..=1440)),
        floor in prop::option::of(prop::option::of(1u32..=90)),
        half_hours in prop::option::of(prop::option::of(1u64..=144)),
    ) {
        let mut args = serde_json::Map::new();
        let mut put = |key: &str, value: Option<Option<Value>>| {
            if let Some(value) = value {
                args.insert(key.into(), value.unwrap_or(Value::Null));
            }
        };
        put("maxJobs", jobs.map(|v| v.map(Value::from)));
        put("idleMinutes", idle.map(|v| v.map(Value::from)));
        put("minFreeMemoryPercent", floor.map(|v| v.map(Value::from)));
        put("maxJobHours", half_hours.map(|v| v.map(|h| Value::from(h as f64 / 2.0))));
        let read = SettingsChange::from_args(Some(&Value::Object(args.clone())));
        if args.is_empty() {
            prop_assert!(read.is_err());
        } else {
            prop_assert_eq!(read, Ok(SettingsChange {
                max_jobs: jobs,
                idle_minutes: idle,
                min_free_memory_percent: floor,
                deadline_minutes: half_hours.map(|v| v.map(|h| h * 30)),
            }));
        }
    }
}
