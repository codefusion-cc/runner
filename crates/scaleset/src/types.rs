use cf_secret::Secret;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// A label of a scale set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    /// `System` for the ones a scale set sets.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The label, as the workflow's `runs-on` names it.
    pub name: String,
}

/// A scale set as the service has it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaleSet {
    /// The service's ID for it.
    pub id: i64,
    /// Its name, unique in the runner group.
    pub name: String,
    /// The labels jobs must ask for.
    #[serde(default)]
    pub labels: Vec<Label>,
}

impl ScaleSet {
    /// The labels, lowercased and sorted, as a set compares.
    pub fn label_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.labels.iter().map(|l| l.name.to_lowercase()).collect();
        names.sort();
        names
    }
}

/// The scale set's job counts as the service reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Statistics {
    /// Jobs offered and not taken by anyone.
    pub total_available_jobs: i64,
    /// Jobs a runner of the scale set acquired and that have no runner yet.
    pub total_acquired_jobs: i64,
    /// Jobs assigned to the scale set, running ones included.
    pub total_assigned_jobs: i64,
    /// Jobs running.
    pub total_running_jobs: i64,
    /// Runners registered with the scale set.
    pub total_registered_runners: i64,
    /// Runners with a job.
    pub total_busy_runners: i64,
    /// Runners without one.
    pub total_idle_runners: i64,
}

impl Statistics {
    /// The assigned jobs, never negative whatever the service sent.
    pub fn assigned(&self) -> usize {
        usize::try_from(self.total_assigned_jobs).unwrap_or(0)
    }
}

/// What the service sends when a message session opens, and what a refresh returns.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionInfo {
    pub session_id: String,
    pub message_queue_url: String,
    pub message_queue_access_token: String,
    #[serde(default)]
    pub statistics: Option<Statistics>,
}

impl std::fmt::Debug for SessionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionInfo")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// A runner registered with the scale set.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunnerRef {
    /// The service's ID for it.
    pub id: i64,
    /// Its name.
    #[serde(default)]
    pub name: String,
}

/// The configuration of one Actions runner that takes one job: it registers itself and goes away after the job.
#[derive(Debug, Clone)]
pub struct JitConfig {
    /// The registered runner.
    pub runner: RunnerRef,
    /// What the Actions runner is started with. A credential: it never reaches a log.
    pub encoded: Secret<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JitResponse {
    pub runner: Option<RunnerRef>,
    #[serde(rename = "encodedJITConfig")]
    pub encoded_jit_config: String,
}

fn lenient_time<'de, D: Deserializer<'de>>(d: D) -> Result<Option<OffsetDateTime>, D::Error> {
    let text: Option<String> = Option::deserialize(d).unwrap_or(None);
    Ok(text.and_then(|t| OffsetDateTime::parse(&t, &Rfc3339).ok()))
}

/// What every job message says about its job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobBase {
    /// The request the job is known by to acquire it.
    pub runner_request_id: i64,
    /// The repository's name without its owner.
    pub repository_name: String,
    /// The repository's owner.
    pub owner_name: String,
    /// The job's name as the run page shows it.
    pub job_display_name: String,
    /// The workflow file at its ref.
    pub job_workflow_ref: String,
    /// The workflow run.
    pub workflow_run_id: i64,
    /// `push`, `pull_request` and so on.
    pub event_name: String,
    /// The labels the job asked for.
    pub request_labels: Vec<String>,
    /// When the job was queued.
    #[serde(deserialize_with = "lenient_time")]
    pub queue_time: Option<OffsetDateTime>,
    /// When a runner got the job.
    #[serde(deserialize_with = "lenient_time")]
    pub runner_assign_time: Option<OffsetDateTime>,
}

impl JobBase {
    /// `owner/name`.
    pub fn repository(&self) -> String {
        if self.owner_name.is_empty() {
            self.repository_name.clone()
        } else {
            format!("{}/{}", self.owner_name, self.repository_name)
        }
    }
}

/// A job the service offers.
pub type JobAvailable = JobBase;

/// A job a runner of the scale set took.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobStarted {
    /// The job.
    #[serde(flatten)]
    pub job: JobBase,
    /// The runner that took it.
    pub runner_name: String,
}

/// A job that is over.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobCompleted {
    /// The job.
    #[serde(flatten)]
    pub job: JobBase,
    /// `succeeded`, `failed`, `canceled`, `skipped` or `abandoned`.
    pub result: String,
    /// The runner that ran it; empty for a job cancelled before any runner had it.
    pub runner_name: String,
}

/// One poll's answer, with the batch in its body read apart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    /// The service's ID for the message; the next poll asks for later ones.
    pub id: i64,
    /// The counts, when the message carried them.
    pub statistics: Option<Statistics>,
    /// Jobs offered.
    pub available: Vec<JobAvailable>,
    /// Jobs acquired by this scale set.
    pub assigned: Vec<JobBase>,
    /// Jobs started on this scale set's runners.
    pub started: Vec<JobStarted>,
    /// Jobs completed.
    pub completed: Vec<JobCompleted>,
    /// Items of the batch that were not understood and left out: a malformed one, or a kind this version does not
    /// know. Their counts still arrive in `statistics`.
    pub skipped: usize,
}

impl Message {
    /// The message a scale set starts with: the counts of the session, nothing else.
    pub fn initial(statistics: Statistics) -> Message {
        Message {
            statistics: Some(statistics),
            ..Message::default()
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawMessage {
    #[serde(default)]
    pub message_id: i64,
    #[serde(default)]
    pub message_type: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub statistics: Option<Statistics>,
}

impl RawMessage {
    /// Reads the batch. A body that is not a list, or items that are not understood, never fail the message: the
    /// counts that came with it still say what to do, and refusing it would bring it back for ever.
    pub(crate) fn into_message(self) -> Message {
        let mut message = Message {
            id: self.message_id,
            statistics: self.statistics,
            ..Message::default()
        };
        if self.message_type != "RunnerScaleSetJobMessages" || self.body.trim().is_empty() {
            return message;
        }
        let items: Vec<Value> = match serde_json::from_str(&self.body) {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!(message = self.message_id, error = %e, "a message body is not a list of jobs; using its counts only");
                message.skipped += 1;
                return message;
            }
        };
        for item in items {
            let kind = item
                .get("messageType")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let understood = match kind.as_str() {
                "JobAvailable" => take(item, &mut message.available, |j| j.runner_request_id > 0),
                "JobAssigned" => take(item, &mut message.assigned, |_| true),
                "JobStarted" => take(item, &mut message.started, |_| true),
                "JobCompleted" => take(item, &mut message.completed, |_| true),
                _ => false,
            };
            if !understood {
                message.skipped += 1;
            }
        }
        message
    }
}

fn take<T: serde::de::DeserializeOwned>(item: Value, into: &mut Vec<T>, valid: impl Fn(&T) -> bool) -> bool {
    match serde_json::from_value::<T>(item) {
        Ok(job) if valid(&job) => {
            into.push(job);
            true
        }
        _ => false,
    }
}

impl std::ops::Deref for JobStarted {
    type Target = JobBase;
    fn deref(&self) -> &JobBase {
        &self.job
    }
}

impl std::ops::Deref for JobCompleted {
    type Target = JobBase;
    fn deref(&self) -> &JobBase {
        &self.job
    }
}
