//! What every test file here starts from: a fake console, a client enrolled with it, and events and reports built by
//! hand from the contract. Raw requests go out through `reqwest` directly, so a request the client would not send can be
//! sent anyway.
#![allow(dead_code)]

use std::time::Duration;

use cf_console_client::{
    Activity, Client, Enroll, Enrolled, HostStatus, JobEvent, JobKind, MachineInfo, Report, ScaleSetStatus,
    Status, enroll,
};
use cf_fake_console::FakeConsole;
use cf_ghapp::{Http, RetryPolicy};
use cf_secret::Secret;
use serde_json::{Value, json};

/// No retries: one call is one request, so what the fake saw is what the client sent.
pub fn no_retry() -> RetryPolicy {
    RetryPolicy {
        retries: 0,
        base: Duration::from_millis(1),
        cap: Duration::from_millis(2),
        max_retry_after: Duration::from_secs(1),
    }
}

/// Retries that take milliseconds.
pub fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        retries: 2,
        base: Duration::from_millis(2),
        cap: Duration::from_millis(5),
        max_retry_after: Duration::from_secs(1),
    }
}

pub fn http() -> Http {
    Http::new(Duration::from_secs(5), no_retry())
}

pub fn hello(code: &str, machine: &str) -> Enroll {
    Enroll {
        code: Secret::new(code.to_string()),
        machine: machine.to_string(),
        hostname: "Build-Mac-2.local".into(),
        model: "Apple M5".into(),
        version: "abc1234".into(),
        cpus: 10,
        memory_bytes: 16 << 30,
        os: Some(cf_console_client::Os::Macos),
    }
}

/// A fake console and a client of it, enrolled as `mac-a`.
pub struct World {
    pub fake: FakeConsole,
    pub client: Client,
    pub token: String,
}

pub async fn world() -> World {
    world_with(http()).await
}

pub async fn world_with(http: Http) -> World {
    let fake = FakeConsole::start().await;
    fake.add_code("code-a");
    let Enrolled { token, name } = enroll(&self::http(), &fake.url(), &hello("code-a", "mac-a"))
        .await
        .expect("enrol");
    assert_eq!(name, "mac-a");
    let token_text = token.expose().clone();
    World {
        client: Client::new(&fake.url(), token, http),
        fake,
        token: token_text,
    }
}

pub fn status(machine: &str) -> Status {
    Status {
        machine: machine.into(),
        version: "abc1234".into(),
        updated_at: "2026-10-10T08:00:00Z".into(),
        open: true,
        reason: None,
        max_jobs: 2,
        memory_for_jobs_bytes: Some(12 << 30),
        paused: false,
        host: Some(HostStatus {
            cpus: 10,
            load1: 2.5,
            load5: 2.0,
            load15: 1.5,
            free_memory_percent: 60.0,
            available_memory_bytes: 10_307_921_510,
            memory_floor_bytes: 2 << 30,
            memory_pressure: 1,
            memory_bytes: 16 << 30,
            swap_used_bytes: 0,
            swap_total_bytes: 0,
            disk_free_bytes: 100 << 30,
            disk_total_bytes: 500 << 30,
            boot_time: Some("2026-10-01T06:00:00Z".into()),
            idle_seconds: -1.0,
        }),
        host_error: None,
        discovery_error: None,
        update: None,
        not_serving: None,
        settings_error: None,
        settings_error_at: None,
        idle_with_jobs_waiting_seconds: 0,
        idle_with_jobs_waiting_reason: None,
        sizes: vec![],
        commands: vec![],
        held_jobs: vec![],
        docker: None,
        scale_sets: vec![ScaleSetStatus {
            repo: "org/app".into(),
            name: "mac-a--app-1a2b".into(),
            id: 4,
            labels: vec!["self-hosted".into()],
            capacity: 1,
            waiting: 0,
            error: None,
            error_at: None,
        }],
        runners: vec![],
        job_caches: vec![],
        job_cache_events: vec![],
    }
}

pub fn report(machine: &str) -> Report {
    Report {
        status: status(machine),
        activity: Activity {
            cpu_percent: 25.0,
            power: None,
        },
        hostname: "Build-Mac-2.local".into(),
        model: "Apple M5".into(),
        os: Some(cf_console_client::Os::Macos),
        settings: None,
        limits: None,
        image: None,
        jobs: vec![],
        results: vec![],
    }
}

pub fn event(n: usize) -> JobEvent {
    JobEvent {
        id: format!("e{n:04}"),
        kind: if n.is_multiple_of(2) {
            JobKind::Started
        } else {
            JobKind::Completed
        },
        runner: format!("mac-a-j{n:04}"),
        repo: Some("org/app".into()),
        job: Some("build".into()),
        workflow: Some(".github/workflows/ci.yml".into()),
        run_id: Some(1000 + n as u64),
        event: Some("push".into()),
        queued_at: None,
        started_at: None,
        at: "2026-10-10T08:00:00Z".into(),
        result: None,
        out_of_memory: false,
        end: Some("timeout".into()),
        machine: Some(MachineInfo {
            size: "cf-4g".into(),
            memory_bytes: 4 << 30,
            limit_bytes: 3 << 30,
            cpus: 4,
            origin: "claimed".into(),
            boot_ms: Some(2100),
            memory_peak_bytes: None,
            disk_peak_bytes: None,
            cpu_usec: None,
            wall_ms: None,
        }),
    }
}

/// A report built straight from the contract (the console's own test helper), as JSON and not as the client's types.
pub fn report_json(machine: &str) -> Value {
    json!({
        "protocol": 1,
        "status": {
            "machine": machine, "version": "abc1234", "updatedAt": "2026-10-10T08:00:00Z", "open": true, "maxJobs": 2,
            "host": {"cpus": 10, "load1": 2.5, "load5": 2, "load15": 1.5, "freeMemoryPercent": 60, "memoryPressure": 1,
                     "memoryBytes": 17179869184u64, "swapUsedBytes": 0, "swapTotalBytes": 0, "diskFreeBytes": 1, "diskTotalBytes": 2,
                     "idleSeconds": -1},
            "scaleSets": [{"repo": "org/app", "name": "mac-a--app-1a2b", "id": 4, "labels": ["self-hosted"], "capacity": 1, "waiting": 0}],
            "runners": null,
        },
        "activity": {"cpuPercent": 25, "power": {"onBattery": false, "percent": 100, "charging": false}},
        "hostname": "Build-Mac-2.local", "model": "Apple M5",
        "settings": {"maxJobs": 2, "idleMinutes": 0, "maxLoadPerCpu": 0.85, "minFreeMemoryPercent": 20, "jobMemoryGb": 4,
                     "jobCpus": 0, "maxJobHours": 6, "vm": {"cpus": 8, "memoryGb": 10, "diskGb": 100}, "autoUpdate": true,
                     "source": "org/runner", "sourceBranch": "main", "repos": null, "publicRepos": null},
        "jobs": null,
        "results": null,
    })
}

pub fn event_json(n: usize) -> Value {
    json!({"kind": "started", "runner": format!("mac-a-j{n}"), "at": "2026-10-10T08:00:00Z"})
}

/// What a raw request got back.
pub struct Raw {
    pub status: u16,
    pub content_type: String,
    pub body: Value,
}

impl Raw {
    pub fn error(&self) -> String {
        self.body["error"].as_str().unwrap_or("").to_string()
    }
}

/// One request, sent as written. `None` leaves the header out.
pub async fn raw(
    base: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Raw {
    let client = reqwest::Client::new();
    let mut request = client.request(method.parse().expect("method"), format!("{base}{path}"));
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let response = request.body(body).send().await.expect("the fake answers");
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response.bytes().await.expect("body");
    Raw {
        status,
        content_type,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

/// A JSON POST with the right headers and the Mac's token.
pub async fn post_json(w: &World, call: &str, body: &Value) -> Raw {
    raw(
        &w.fake.url(),
        "POST",
        &format!("/agent/runners/{call}"),
        Some(&format!("Bearer {}", w.token)),
        Some("application/json"),
        serde_json::to_vec(body).expect("json"),
    )
    .await
}

/// A connect request, with a fresh code the console knows.
pub async fn post_hello(fake: &FakeConsole, body: &Value) -> Raw {
    raw(
        &fake.url(),
        "POST",
        "/agent/runners/enroll",
        None,
        Some("application/json"),
        serde_json::to_vec(body).expect("json"),
    )
    .await
}

pub fn hello_json(code: &str, machine: &str) -> Value {
    json!({"protocol": 1, "code": code, "machine": machine, "hostname": "Build-Mac-2.local", "model": "Apple M5",
           "version": "abc1234", "cpus": 10, "memoryBytes": 17179869184u64})
}

pub fn ids(events: &[JobEvent]) -> Vec<String> {
    events.iter().map(|e| e.id.clone()).collect()
}
