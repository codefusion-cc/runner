//! protocol.ts, field by field: what the console's zod schemas accept and refuse, checked on a parsed JSON value.
//!
//! The console reads strictly enough that nothing unbounded reaches its database, and loosely enough that unknown
//! fields are dropped. A text field (`text(n)` there) takes up to 4n characters and cuts the rest to n; a strict text
//! field takes exactly its bound. Lengths count UTF-16 units, as JavaScript does. `.default(x)` takes an absent field
//! but not `null`; `.nullish()` takes both; Go's habit of writing empty lists as `null` is a `list`.

use serde_json::{Map, Value};

/// The first thing wrong, as the console's `firstProblem` words it: `path: why`.
#[derive(Debug)]
pub(crate) struct Problem {
    path: String,
    why: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = if self.path.is_empty() { "body" } else { &self.path };
        write!(f, "{path}: {}", self.why)
    }
}

type Check = Result<(), Problem>;

fn bad(path: &str, why: &str) -> Problem {
    Problem {
        path: path.to_string(),
        why: why.to_string(),
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Presence {
    /// The field must be there.
    Required,
    /// `.default(x)`: absent is fine, `null` is checked like any value.
    Default,
    /// `.nullish()`: absent or `null`.
    Nullish,
}

use Presence::{Default, Nullish, Required};

fn field(
    o: &Map<String, Value>,
    path: &str,
    key: &str,
    presence: Presence,
    check: impl FnOnce(&Value, &str) -> Check,
) -> Check {
    let here = join(path, key);
    match o.get(key) {
        None if presence == Required => {
            Err(bad(&here, "Invalid input: expected a value, received undefined"))
        }
        None => Ok(()),
        Some(Value::Null) if presence == Nullish => Ok(()),
        Some(value) => check(value, &here),
    }
}

fn object<'a>(v: &'a Value, path: &str) -> Result<&'a Map<String, Value>, Problem> {
    v.as_object()
        .ok_or_else(|| bad(path, "Invalid input: expected object"))
}

fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `z.string().min(min).max(max)`.
fn string(v: &Value, path: &str, min: usize, max: usize) -> Check {
    let s = v
        .as_str()
        .ok_or_else(|| bad(path, "Invalid input: expected string"))?;
    match units(s) {
        n if n < min => Err(bad(path, "Too small")),
        n if n > max => Err(bad(path, "Too big")),
        _ => Ok(()),
    }
}

/// `text(max)` and `optionalText(max)`: any string up to four times the bound, which the console cuts to the bound.
fn text(max: usize) -> impl Fn(&Value, &str) -> Check {
    move |v, path| string(v, path, 0, max * 4)
}

fn number(v: &Value, path: &str, min: f64, max: f64) -> Check {
    let n = v
        .as_f64()
        .ok_or_else(|| bad(path, "Invalid input: expected number"))?;
    if n < min {
        Err(bad(path, "Too small"))
    } else if n > max {
        Err(bad(path, "Too big"))
    } else {
        Ok(())
    }
}

/// `z.number().int()`: a safe integer (JSON `2.0` is one, as it is in JavaScript).
fn int(v: &Value, path: &str, min: f64, max: f64) -> Check {
    number(v, path, min, max)?;
    let n = v.as_f64().unwrap_or(0.0);
    if n.fract() != 0.0 || n.abs() > 9_007_199_254_740_991.0 {
        return Err(bad(path, "Invalid input: expected int"));
    }
    Ok(())
}

fn count(v: &Value, path: &str) -> Check {
    int(v, path, 0.0, 1_000_000.0)
}

fn bytes(v: &Value, path: &str) -> Check {
    number(v, path, 0.0, 9_007_199_254_740_992.0)
}

fn boolean(v: &Value, path: &str) -> Check {
    if v.is_boolean() {
        Ok(())
    } else {
        Err(bad(path, "Invalid input: expected boolean"))
    }
}

/// `time`: a string of at most 64 characters that `Date.parse` reads. Here: ISO 8601 (`2026-10-10`, with or without
/// a time of day, a fraction and a zone, `T` or a space between), which is all the Runner writes.
fn time(v: &Value, path: &str) -> Check {
    string(v, path, 0, 64)?;
    if is_time(v.as_str().unwrap_or("")) {
        Ok(())
    } else {
        Err(bad(path, "a time"))
    }
}

/// `optionalTime`: a string of at most 64 characters; one that is no time reads as none.
fn optional_time(v: &Value, path: &str) -> Check {
    string(v, path, 0, 64)
}

fn digits(b: &[u8], at: &mut usize, n: usize) -> Option<u32> {
    let part = b.get(*at..*at + n)?;
    if !part.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *at += n;
    Some(part.iter().fold(0, |acc, d| acc * 10 + u32::from(d - b'0')))
}

fn eat(b: &[u8], at: &mut usize, c: u8) -> bool {
    let found = b.get(*at) == Some(&c);
    if found {
        *at += 1;
    }
    found
}

fn is_time(s: &str) -> bool {
    let b = s.as_bytes();
    let mut at = 0;
    parse_time(b, &mut at).is_some() && at == b.len()
}

fn parse_time(b: &[u8], at: &mut usize) -> Option<()> {
    let year = digits(b, at, 4)?;
    eat(b, at, b'-').then_some(())?;
    let month = digits(b, at, 2)?;
    eat(b, at, b'-').then_some(())?;
    let day = digits(b, at, 2)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > days {
        return None;
    }
    if *at == b.len() {
        return Some(());
    }
    (eat(b, at, b'T') || eat(b, at, b' ')).then_some(())?;
    let hour = digits(b, at, 2)?;
    eat(b, at, b':').then_some(())?;
    let minute = digits(b, at, 2)?;
    let mut second = 0;
    if eat(b, at, b':') {
        second = digits(b, at, 2)?;
        if eat(b, at, b'.') {
            let start = *at;
            while b.get(*at).is_some_and(u8::is_ascii_digit) {
                *at += 1;
            }
            (*at > start).then_some(())?;
        }
    }
    if minute > 59 || second > 59 || hour > 24 || (hour == 24 && (minute != 0 || second != 0)) {
        return None;
    }
    if !eat(b, at, b'Z') && b.get(*at).is_some_and(|c| *c == b'+' || *c == b'-') {
        *at += 1;
        let zone_hour = digits(b, at, 2)?;
        eat(b, at, b':');
        let zone_minute = digits(b, at, 2)?;
        if zone_hour > 23 || zone_minute > 59 {
            return None;
        }
    }
    Some(())
}

/// `list(item, max)`: an array of at most `max` items (with `Nullish`, `null` is an empty list).
fn list(max: usize, item: impl Fn(&Value, &str) -> Check) -> impl Fn(&Value, &str) -> Check {
    move |v, path| {
        let items = v
            .as_array()
            .ok_or_else(|| bad(path, "Invalid input: expected array"))?;
        if items.len() > max {
            return Err(bad(path, "Too big"));
        }
        items
            .iter()
            .enumerate()
            .try_for_each(|(i, value)| item(value, &join(path, &i.to_string())))
    }
}

/// `MACHINE_NAME` and `MACHINE_NAME_MAX` of shared/runners.ts.
fn machine_name(v: &Value, path: &str) -> Check {
    string(v, path, 0, 24)?;
    let s = v.as_str().unwrap_or("");
    let shaped = s
        .split('-')
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    if shaped {
        Ok(())
    } else {
        Err(bad(path, "Invalid string: must match pattern"))
    }
}

fn literal_protocol(v: &Value, path: &str) -> Check {
    if v.as_f64() == Some(1.0) {
        Ok(())
    } else {
        Err(bad(path, "Invalid input: expected 1"))
    }
}

/// `helloSchema`.
pub(crate) fn hello(v: &Value) -> Check {
    let o = object(v, "")?;
    field(o, "", "protocol", Required, literal_protocol)?;
    field(o, "", "code", Required, |v, p| string(v, p, 1, 200))?;
    field(o, "", "machine", Required, machine_name)?;
    field(o, "", "hostname", Default, text(255))?;
    field(o, "", "model", Default, text(200))?;
    field(o, "", "version", Default, text(64))?;
    field(o, "", "cpus", Required, |v, p| int(v, p, 1.0, 4096.0))?;
    field(o, "", "memoryBytes", Required, bytes)
}

fn host(v: &Value, path: &str) -> Check {
    let o = object(v, path)?;
    field(o, path, "cpus", Required, |v, p| int(v, p, 0.0, 4096.0))?;
    field(o, path, "load1", Required, |v, p| number(v, p, 0.0, 1e6))?;
    field(o, path, "load5", Default, |v, p| number(v, p, 0.0, 1e6))?;
    field(o, path, "load15", Default, |v, p| number(v, p, 0.0, 1e6))?;
    field(o, path, "freeMemoryPercent", Required, |v, p| {
        number(v, p, 0.0, 100.0)
    })?;
    field(o, path, "memoryPressure", Default, |v, p| int(v, p, 0.0, 8.0))?;
    for key in [
        "memoryBytes",
        "swapUsedBytes",
        "swapTotalBytes",
        "diskFreeBytes",
        "diskTotalBytes",
    ] {
        field(o, path, key, Default, bytes)?;
    }
    field(o, path, "bootTime", Nullish, optional_time)?;
    field(o, path, "idleSeconds", Default, |v, p| number(v, p, -1.0, 1e9))
}

fn runner(v: &Value, path: &str) -> Check {
    let o = object(v, path)?;
    field(o, path, "name", Required, text(100))?;
    field(o, path, "scaleSet", Required, |v, p| {
        int(v, p, f64::MIN, f64::MAX)
    })?;
    field(o, path, "state", Required, text(20))?;
    field(o, path, "started", Required, time)?;
    field(o, path, "job", Nullish, |v, p| {
        let j = object(v, p)?;
        field(j, p, "repo", Default, text(200))?;
        field(j, p, "name", Default, text(300))?;
        field(j, p, "workflow", Default, text(400))?;
        field(j, p, "runId", Default, |v, p| int(v, p, 0.0, f64::MAX))?;
        field(j, p, "started", Required, time)
    })?;
    field(o, path, "usage", Nullish, |v, p| {
        let u = object(v, p)?;
        field(u, p, "cpuPercent", Required, |v, p| number(v, p, -1.0, 1e5))?;
        field(u, p, "memoryBytes", Required, bytes)
    })
}

fn scale_set(v: &Value, path: &str) -> Check {
    let o = object(v, path)?;
    field(o, path, "repo", Required, text(200))?;
    field(o, path, "name", Required, text(100))?;
    field(o, path, "id", Required, |v, p| int(v, p, f64::MIN, f64::MAX))?;
    field(o, path, "labels", Nullish, list(30, text(100)))?;
    field(o, path, "capacity", Required, count)?;
    field(o, path, "waiting", Default, count)?;
    field(o, path, "error", Nullish, text(1000))?;
    field(o, path, "errorAt", Nullish, optional_time)
}

fn update(v: &Value, path: &str) -> Check {
    let o = object(v, path)?;
    field(o, path, "source", Default, text(200))?;
    field(o, path, "movedFrom", Nullish, text(200))?;
    field(o, path, "checkedAt", Nullish, optional_time)?;
    field(o, path, "error", Nullish, text(1000))?;
    field(o, path, "errorAt", Nullish, optional_time)?;
    field(o, path, "failures", Default, count)
}

fn status(v: &Value, path: &str) -> Check {
    let o = object(v, path)?;
    field(o, path, "machine", Required, machine_name)?;
    field(o, path, "version", Default, text(64))?;
    field(o, path, "updatedAt", Required, time)?;
    field(o, path, "open", Required, boolean)?;
    field(o, path, "reason", Nullish, text(300))?;
    field(o, path, "maxJobs", Required, count)?;
    field(o, path, "paused", Default, boolean)?;
    field(o, path, "host", Nullish, host)?;
    field(o, path, "hostError", Nullish, text(300))?;
    field(o, path, "discoveryError", Nullish, text(1000))?;
    field(o, path, "update", Nullish, update)?;
    field(o, path, "notServing", Nullish, text(300))?;
    field(o, path, "scaleSets", Nullish, list(500, scale_set))?;
    field(o, path, "runners", Nullish, list(64, runner))
}

fn settings(v: &Value, path: &str) -> Check {
    let o = object(v, path)?;
    field(o, path, "maxJobs", Required, count)?;
    field(o, path, "idleMinutes", Required, count)?;
    field(o, path, "maxLoadPerCpu", Required, |v, p| {
        number(v, p, 0.0, 1000.0)
    })?;
    field(o, path, "minFreeMemoryPercent", Required, |v, p| {
        number(v, p, 0.0, 100.0)
    })?;
    field(o, path, "jobMemoryGb", Required, |v, p| number(v, p, 0.0, 1e4))?;
    field(o, path, "jobCpus", Required, |v, p| number(v, p, 0.0, 4096.0))?;
    field(o, path, "maxJobHours", Required, |v, p| number(v, p, 0.0, 1e4))?;
    field(o, path, "vm", Required, |v, p| {
        let vm = object(v, p)?;
        for key in ["cpus", "memoryGb", "diskGb"] {
            field(vm, p, key, Required, count)?;
        }
        Ok(())
    })?;
    field(o, path, "autoUpdate", Required, boolean)?;
    field(o, path, "source", Default, text(200))?;
    field(o, path, "sourceBranch", Default, text(200))?;
    field(o, path, "repos", Nullish, list(200, text(200)))?;
    field(o, path, "publicRepos", Nullish, list(200, text(200)))
}

/// `jobEvent`. `outOfMemory` is `z.boolean().catch(false)`: any value is read, and none is refused.
fn job_event(v: &Value, path: &str) -> Check {
    let o = object(v, path)?;
    field(o, path, "kind", Required, |v, p| match v.as_str() {
        Some("started" | "completed") => Ok(()),
        _ => Err(bad(
            p,
            "Invalid option: expected one of \"started\"|\"completed\"",
        )),
    })?;
    // The one text here with a hard bound, z.string().min(1).max(100), and no cutting.
    field(o, path, "runner", Required, |v, p| string(v, p, 1, 100))?;
    field(o, path, "repo", Nullish, text(200))?;
    field(o, path, "job", Nullish, text(300))?;
    field(o, path, "workflow", Nullish, text(400))?;
    field(o, path, "runId", Nullish, |v, p| int(v, p, 0.0, f64::MAX))?;
    field(o, path, "event", Nullish, text(50))?;
    field(o, path, "queuedAt", Nullish, optional_time)?;
    field(o, path, "startedAt", Nullish, optional_time)?;
    field(o, path, "at", Required, time)?;
    field(o, path, "result", Nullish, text(30))
}

fn command_result(v: &Value, path: &str) -> Check {
    let o = object(v, path)?;
    field(o, path, "id", Required, |v, p| string(v, p, 1, 64))?;
    field(o, path, "ok", Required, boolean)?;
    field(o, path, "message", Default, text(1000))?;
    field(o, path, "at", Required, time)
}

/// `reportSchema`.
pub(crate) fn report(v: &Value) -> Check {
    let o = object(v, "")?;
    field(o, "", "protocol", Required, literal_protocol)?;
    field(o, "", "status", Required, status)?;
    field(o, "", "activity", Default, |v, p| {
        let a = object(v, p)?;
        field(a, p, "cpuPercent", Default, |v, p| number(v, p, -1.0, 100.0))?;
        field(a, p, "power", Nullish, |v, p| {
            let w = object(v, p)?;
            field(w, p, "onBattery", Required, boolean)?;
            field(w, p, "percent", Required, |v, p| int(v, p, 0.0, 100.0))?;
            field(w, p, "charging", Required, boolean)
        })
    })?;
    field(o, "", "hostname", Default, text(255))?;
    field(o, "", "model", Default, text(200))?;
    field(o, "", "settings", Nullish, settings)?;
    field(o, "", "image", Nullish, |v, p| {
        let i = object(v, p)?;
        field(i, p, "commit", Default, text(64))?;
        field(i, p, "built", Required, time)
    })?;
    field(o, "", "jobs", Nullish, list(500, job_event))?;
    field(o, "", "results", Nullish, list(50, command_result))
}
