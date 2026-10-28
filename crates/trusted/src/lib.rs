//! The trusted class: a host that runs only main's deploy, tag and signing jobs of one repository, and receives the
//! production tokens those jobs hold. Pull requests run workflows from their own head, so a pull request can ask for
//! the label `trusted` and be handed to such a host: this crate decides, from the job message GitHub sent the Actions
//! runner and nothing else, whether a job may run there.
//!
//! [`Policy`] is the allow-list as the configuration file has it, checked each time it is used ([`Policy::rules`]):
//! one that is missing, empty or malformed refuses every job. [`read_run`] takes the five facts of the run from the
//! message's `contextData.github`, refusing a message that leaves one out or says one twice. [`decide`] joins them.
//! Pure: no I/O, no clock, no environment.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The repository whose jobs a trusted host runs when its configuration names none.
pub const DEFAULT_REPOSITORY: &str = "codefusion-cc/codefusion";
/// The only ref a trusted job runs on: main itself, never a tag or another branch of a similar name.
pub const MAIN_REF: &str = "refs/heads/main";
/// The events an allow-list may name. None of them can be started by a pull request, a comment, a merge queue or
/// another workflow's run: `pull_request`, `pull_request_target`, `issue_comment`, `merge_group` and `workflow_run`
/// are refused in an allow-list, as is anything not named here.
pub const EVENTS: [&str; 3] = ["push", "schedule", "workflow_dispatch"];
/// The most workflows an allow-list names.
pub const MAX_WORKFLOWS: usize = 32;
/// The longest workflow file name and repository name.
const MAX_NAME: usize = 100;
/// How much of a value from the job message a refusal quotes.
const QUOTE_MAX: usize = 120;

/// The workflows (by file name) and events the trusted host runs when its configuration names none: codefusion's
/// production deploy, its npm release and tags, and Magnetar's signing and publishing. A reusable workflow's jobs
/// run under its caller's `github.workflow_ref`, so Magnetar's release on a push to main is `magnetar-main.yml`.
pub const DEFAULT_ALLOW: [(&str, &[&str]); 5] = [
    ("deploy-production.yml", &["workflow_dispatch"]),
    ("release.yml", &["push", "workflow_dispatch"]),
    ("magnetar-main.yml", &["push"]),
    ("magnetar-release.yml", &["workflow_dispatch"]),
    ("magnetar-preview-app.yml", &["workflow_dispatch"]),
];

/// The trusted class's rules as the configuration file has them: `{"repository": "owner/name", "allow": {"<file>":
/// ["<event>", ...]}}`. Kept as written and checked whenever it is used, so a malformed one reaches the guest as it is
/// and refuses every job there with the reason, rather than stopping the Runner that would report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Policy(Box<Value>);

impl Default for Policy {
    fn default() -> Self {
        Policy::defaults()
    }
}

impl Policy {
    /// [`DEFAULT_REPOSITORY`] and [`DEFAULT_ALLOW`].
    pub fn defaults() -> Policy {
        let allow: serde_json::Map<String, Value> = DEFAULT_ALLOW
            .iter()
            .map(|(file, events)| (file.to_string(), json!(events)))
            .collect();
        Policy(Box::new(
            json!({ "repository": DEFAULT_REPOSITORY, "allow": allow }),
        ))
    }

    /// The rules as written, whatever they are.
    pub fn new(value: Value) -> Policy {
        Policy(Box::new(value))
    }

    /// The repository whose jobs the host takes: the rules' own, or [`DEFAULT_REPOSITORY`] when they name none.
    /// Read apart from the rest, so a host whose allow-list is malformed still takes that repository's jobs, to
    /// refuse each with the reason.
    pub fn repository(&self) -> Result<String, String> {
        let Value::Object(fields) = &*self.0 else {
            return Err("the trusted rules are not an object".into());
        };
        match fields.get("repository") {
            None => Ok(DEFAULT_REPOSITORY.to_string()),
            Some(Value::String(name)) if is_repository(name) => Ok(name.clone()),
            Some(other) => Err(format!("repository: {} is not owner/name", quote_value(other))),
        }
    }

    /// The rules, or why they refuse every job: not an object, a field this version does not know, a repository
    /// that is not `owner/name`, an allow-list that is missing, empty, too long, names a file that is not a plain
    /// `.yml` or `.yaml` name, or an event outside [`EVENTS`].
    pub fn rules(&self) -> Result<Rules, String> {
        let Value::Object(fields) = &*self.0 else {
            return Err("the trusted rules are not an object".into());
        };
        if let Some(unknown) = fields
            .keys()
            .find(|key| !matches!(key.as_str(), "repository" | "allow"))
        {
            return Err(format!(
                "the trusted rules have a field {} this Runner does not know",
                quote(unknown)
            ));
        }
        let repository = self.repository()?;
        let Some(allow) = fields.get("allow") else {
            return Err("there is no allow-list".into());
        };
        let Value::Object(allow) = allow else {
            return Err("allow: not an object of workflow files and their events".into());
        };
        if allow.is_empty() {
            return Err("the allow-list is empty".into());
        }
        if allow.len() > MAX_WORKFLOWS {
            return Err(format!(
                "the allow-list names {} workflows, more than {MAX_WORKFLOWS}",
                allow.len()
            ));
        }
        let mut rules = BTreeMap::new();
        for (file, events) in allow {
            if !is_workflow_file(file) {
                return Err(format!(
                    "allow: {} is not the file name of a workflow in .github/workflows (letters, digits, `.`, `_`, \
                     `-`, ending in .yml or .yaml)",
                    quote(file)
                ));
            }
            let Value::Array(events) = events else {
                return Err(format!("allow: {}: not a list of events", quote(file)));
            };
            if events.is_empty() {
                return Err(format!("allow: {}: no events", quote(file)));
            }
            let mut allowed = BTreeSet::new();
            for event in events {
                match event {
                    Value::String(event) if EVENTS.contains(&event.as_str()) => {
                        allowed.insert(event.clone());
                    }
                    other => {
                        return Err(format!(
                            "allow: {}: {} is not one of {}",
                            quote(file),
                            quote_value(other),
                            EVENTS.join(", ")
                        ));
                    }
                }
            }
            rules.insert(file.clone(), allowed);
        }
        Ok(Rules {
            repository,
            allow: rules,
        })
    }
}

/// An allow-list that holds: the repository whose jobs run here, and each workflow file with the events that may
/// start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    /// `owner/name`.
    pub repository: String,
    /// Workflow file name to the events that may start it.
    pub allow: BTreeMap<String, BTreeSet<String>>,
}

/// What the job message says of the run a job belongs to: `github.repository`, `github.ref`, `github.workflow_ref`,
/// `github.event_name` and `github.run_attempt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// `owner/name` of the repository the run is in.
    pub repository: String,
    /// The ref the run is on: `refs/heads/main`, `refs/pull/7/merge`, `refs/tags/v1`.
    pub git_ref: String,
    /// The workflow file at its ref, `owner/name/.github/workflows/<file>@<ref>`; a reusable workflow's caller's.
    pub workflow_ref: String,
    /// What started the run: `push`, `workflow_dispatch`, `pull_request` and so on.
    pub event_name: String,
    /// Which attempt of the run: `1` unless it was re-run.
    pub run_attempt: String,
}

/// A job the rules let run: the workflow file and the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The workflow file on the allow-list.
    pub workflow: String,
    /// The event that started the run.
    pub event: String,
}

impl fmt::Display for Accepted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} on {}", self.workflow, self.event)
    }
}

impl Rules {
    /// Whether a run may have a job here: every reason it may not, or the workflow and event it runs as.
    pub fn check(&self, run: &Run) -> Result<Accepted, Vec<String>> {
        let mut reasons = Vec::new();
        if run.repository != self.repository {
            reasons.push(format!(
                "the job is of {}, and this machine runs jobs of {} only",
                quote(&run.repository),
                self.repository
            ));
        }
        if run.git_ref != MAIN_REF {
            reasons.push(format!(
                "the run is on {}, and this machine runs main's ({MAIN_REF}) only",
                quote(&run.git_ref)
            ));
        }
        if run.run_attempt != "1" {
            reasons.push(format!(
                "this is attempt {} of the run, and a re-run is refused as production's gate refuses it: start a new run",
                quote(&run.run_attempt)
            ));
        }
        let prefix = format!("{}/.github/workflows/", self.repository);
        let file = run
            .workflow_ref
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(&format!("@{MAIN_REF}")))
            .filter(|file| is_workflow_file(file));
        let mut accepted = None;
        match file.and_then(|file| self.allow.get_key_value(file)) {
            None if file.is_none() => reasons.push(format!(
                "its workflow is {}, not a workflow file of {}'s main",
                quote(&run.workflow_ref),
                self.repository
            )),
            None => reasons.push(format!(
                "its workflow {} is not on this machine's allow-list ({})",
                quote(file.unwrap_or_default()),
                self.allow.keys().cloned().collect::<Vec<_>>().join(", ")
            )),
            Some((file, events)) if !events.contains(&run.event_name) => reasons.push(format!(
                "{file} may run here when {} starts it, and this run was started by {}",
                events.iter().cloned().collect::<Vec<_>>().join(" or "),
                quote(&run.event_name)
            )),
            Some((file, _)) => {
                accepted = Some(Accepted {
                    workflow: file.clone(),
                    event: run.event_name.clone(),
                })
            }
        }
        match accepted {
            Some(accepted) if reasons.is_empty() => Ok(accepted),
            _ => Err(reasons),
        }
    }
}

/// Whether the trusted host may run the job of `message`, the job message GitHub's Actions runner received
/// (`AgentJobRequestMessage` as JSON): the workflow and event, or a refusal that says every reason.
pub fn decide(policy: &Policy, message: &str) -> Result<Accepted, String> {
    let rules = policy
        .rules()
        .map_err(|why| format!("this machine's allow-list does not hold ({why}), so it refuses every job"))?;
    let run = read_run(message).map_err(|why| format!("the job message cannot be read: {why}"))?;
    rules.check(&run).map_err(|reasons| reasons.join("; "))
}

/// The run's five facts from a job message's `contextData.github`. The message is GitHub's, through the Actions
/// runner, as the Actions runner's Worker would read it: dictionaries by name with case ignored, so a name that
/// comes twice, in any case, is refused rather than guessed. A fact left out, or that is not a text, is refused;
/// `run_attempt` may also be a whole number.
pub fn read_run(message: &str) -> Result<Run, String> {
    let top: Entries =
        serde_json::from_str(message).map_err(|error| format!("not a JSON object ({error})"))?;
    let context = top.one("contextData")?.ok_or("it has no contextData")?;
    let context: Entries = Entries::deserialize(context).map_err(|error| format!("contextData: {error}"))?;
    let github = context.one("github")?.ok_or("contextData has no github")?;
    let github = dictionary(github).map_err(|why| format!("contextData.github: {why}"))?;
    let text = |name: &str| -> Result<String, String> {
        match github.one(name)? {
            Some(Value::String(text)) => Ok(text.clone()),
            Some(_) => Err(format!("github.{name} is not a text")),
            None => Err(format!("github.{name} is missing")),
        }
    };
    let run_attempt = match github.one("run_attempt")? {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => whole(number).ok_or("github.run_attempt is not a whole number")?,
        Some(_) => return Err("github.run_attempt is not a text".into()),
        None => return Err("github.run_attempt is missing".into()),
    };
    Ok(Run {
        repository: text("repository")?,
        git_ref: text("ref")?,
        workflow_ref: text("workflow_ref")?,
        event_name: text("event_name")?,
        run_attempt,
    })
}

/// A whole number as the Actions runner writes a number (`2.0`), in decimal.
fn whole(number: &serde_json::Number) -> Option<String> {
    if let Some(n) = number.as_u64() {
        return Some(n.to_string());
    }
    let n = number.as_f64()?;
    (n.fract() == 0.0 && (0.0..1e15).contains(&n)).then(|| format!("{n:.0}"))
}

/// A dictionary of the Actions runner's context data: `{"t": 2, "d": [{"k": name, "v": value}, ...]}`, with no
/// `d` when it is empty.
fn dictionary(value: &Value) -> Result<Entries, String> {
    let fields: Entries = Entries::deserialize(value).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for (name, value) in &fields.0 {
        match (name.as_str(), value) {
            ("t", Value::Number(kind)) if kind.as_u64() == Some(2) => {}
            ("t", _) => return Err("not a dictionary (t is not 2)".into()),
            ("d", Value::Array(items)) => {
                for item in items {
                    let pair: Entries = Entries::deserialize(item).map_err(|error| error.to_string())?;
                    let (Some(Value::String(key)), Some(value), 2) =
                        (pair.exact("k"), pair.exact("v"), pair.0.len())
                    else {
                        return Err("an entry is not {k, v}".into());
                    };
                    entries.push((key.clone(), value.clone()));
                }
            }
            ("d", _) => return Err("its d is not a list".into()),
            (other, _) => return Err(format!("a field {} a dictionary does not have", quote(other))),
        }
    }
    if !fields.0.iter().any(|(name, _)| name == "t") {
        return Err("not a dictionary (no t)".into());
    }
    Ok(Entries(entries))
}

/// A JSON object's fields in order, duplicates kept, so [`Entries::one`] can refuse a name given twice where serde's
/// own map would keep the last.
#[derive(Debug)]
struct Entries(Vec<(String, Value)>);

impl Entries {
    /// The value of `name`, with case ignored as the Actions runner's dictionaries ignore it; an error when it comes
    /// more than once.
    fn one(&self, name: &str) -> Result<Option<&Value>, String> {
        let mut found = self.0.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name));
        let first = found.next().map(|(_, value)| value);
        if found.next().is_some() {
            return Err(format!("{name} is there more than once"));
        }
        Ok(first)
    }

    /// The value of exactly `name`, when it comes once.
    fn exact(&self, name: &str) -> Option<&Value> {
        let mut found = self.0.iter().filter(|(key, _)| key == name);
        let first = found.next().map(|(_, value)| value);
        found.next().is_none().then_some(first).flatten()
    }
}

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Entries;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Entries, A::Error> {
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    entries.push((key, value));
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(Fields)
    }
}

/// `owner/name` of the characters GitHub allows.
pub fn is_repository(name: &str) -> bool {
    let plain = |part: &str| {
        !part.is_empty()
            && part.len() <= MAX_NAME
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && !matches!(part, "." | "..")
    };
    matches!(name.split_once('/'), Some((owner, repo)) if plain(owner) && plain(repo))
}

/// A workflow's file name in `.github/workflows`: no folder, letters, digits, `.`, `_` and `-`, ending in `.yml` or
/// `.yaml`.
pub fn is_workflow_file(name: &str) -> bool {
    let stem = name.strip_suffix(".yml").or_else(|| name.strip_suffix(".yaml"));
    stem.is_some_and(|stem| {
        !stem.is_empty()
            && name.len() <= MAX_NAME
            && !stem.starts_with('.')
            && stem
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    })
}

/// A value from a job message or a configuration, quoted with its control characters escaped and cut to a length
/// a log line takes.
fn quote(text: &str) -> String {
    let cut: String = text.chars().take(QUOTE_MAX).collect();
    let more = if cut.len() < text.len() { "..." } else { "" };
    format!("{:?}{more}", cut)
}

fn quote_value(value: &Value) -> String {
    match value {
        Value::String(text) => quote(text),
        other => quote(&other.to_string()),
    }
}
