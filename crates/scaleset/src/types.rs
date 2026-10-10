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
    #[serde(default, deserialize_with = "null_default")]
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

/// A field the service may send as `null`, which Go's decoder reads as the empty value and serde's does not.
pub(crate) fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn lenient_time<'de, D: Deserializer<'de>>(d: D) -> Result<Option<OffsetDateTime>, D::Error> {
    let text: Option<String> = Option::deserialize(d).unwrap_or(None);
    // The service writes a time it does not have as the year 1.
    Ok(text
        .and_then(|t| OffsetDateTime::parse(&t, &Rfc3339).ok())
        .filter(|t| t.year() > 1))
}

/// What every job message says about its job. A field the service leaves out or sends as `null` is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobBase {
    /// The request the job is known by to acquire it.
    #[serde(deserialize_with = "null_default")]
    pub runner_request_id: i64,
    /// The repository's name without its owner.
    #[serde(deserialize_with = "null_default")]
    pub repository_name: String,
    /// The repository's owner.
    #[serde(deserialize_with = "null_default")]
    pub owner_name: String,
    /// The job's name as the run page shows it.
    #[serde(deserialize_with = "null_default")]
    pub job_display_name: String,
    /// The workflow file at its ref.
    #[serde(deserialize_with = "null_default")]
    pub job_workflow_ref: String,
    /// The workflow run.
    #[serde(deserialize_with = "null_default")]
    pub workflow_run_id: i64,
    /// `push`, `pull_request` and so on.
    #[serde(deserialize_with = "null_default")]
    pub event_name: String,
    /// The labels the job asked for.
    #[serde(deserialize_with = "null_default")]
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
    #[serde(deserialize_with = "null_default")]
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
    #[serde(deserialize_with = "null_default")]
    pub result: String,
    /// The runner that ran it; empty for a job cancelled before any runner had it.
    #[serde(deserialize_with = "null_default")]
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
    /// The batch as the service sent it, for a log line when something is not as expected. Job names and
    /// repositories, never a credential.
    pub raw: String,
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
    #[serde(default, deserialize_with = "null_default")]
    pub message_type: String,
    #[serde(default, deserialize_with = "null_default")]
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
            raw: self.body.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn message(body: &str) -> Message {
        RawMessage {
            message_id: 1,
            message_type: "RunnerScaleSetJobMessages".into(),
            body: body.into(),
            statistics: None,
        }
        .into_message()
    }

    // What the real service sent for a job it assigned by itself (seen 2026-10-10): no request ID, zero times.
    #[test]
    fn the_real_services_assigned_job_has_no_request_id_and_no_times() {
        let m = message(
            r#"[{"messageType":"JobAssigned","repositoryName":"r","ownerName":"o","requestLabels":["self-hosted","cf-2g"],
            "scaleSetAssignTime":"2026-10-10T17:22:32.94457976Z","runnerAssignTime":"0001-01-01T00:00:00Z",
            "queueTime":"0001-01-01T00:00:00Z","runnerRequestId":0,"workflowRunId":38071473638}]"#,
        );
        assert_eq!((m.assigned.len(), m.skipped), (1, 0));
        assert_eq!(m.assigned[0].runner_request_id, 0);
        assert_eq!(m.assigned[0].queue_time, None);
        assert_eq!(m.assigned[0].runner_assign_time, None);
    }

    #[test]
    fn null_fields_read_as_empty_like_go_reads_them() {
        let raw: RawMessage =
            serde_json::from_str(r#"{"messageId":3,"messageType":null,"body":null,"statistics":null}"#)
                .unwrap();
        let m = raw.into_message();
        assert_eq!((m.id, m.skipped, m.raw.as_str()), (3, 0, ""));
        let set: ScaleSet = serde_json::from_str(r#"{"id":1,"name":"a","labels":null}"#).unwrap();
        assert!(set.labels.is_empty());
    }

    #[test]
    fn a_job_with_null_fields_is_still_a_job() {
        let m = message(
            r#"[{"messageType":"JobAvailable","runnerRequestId":7,"requestLabels":null,"eventName":null,"queueTime":null},
            {"messageType":"JobCompleted","runnerRequestId":7,"result":"canceled","runnerName":null,"jobDisplayName":null},
            {"messageType":"JobStarted","runnerRequestId":8,"runnerName":null,"workflowRunId":null}]"#,
        );
        assert_eq!(
            (m.available.len(), m.completed.len(), m.started.len(), m.skipped),
            (1, 1, 1, 0)
        );
        assert_eq!(m.available[0].runner_request_id, 7);
        assert!(m.available[0].request_labels.is_empty());
        assert_eq!(
            (
                m.completed[0].result.as_str(),
                m.completed[0].runner_name.as_str()
            ),
            ("canceled", "")
        );
    }

    #[test]
    fn an_offer_without_a_request_id_is_left_out_because_it_cannot_be_acquired() {
        let m = message(r#"[{"messageType":"JobAvailable","runnerRequestId":0}]"#);
        assert_eq!((m.available.len(), m.skipped), (0, 1));
    }

    proptest! {
        #[test]
        fn no_body_is_a_panic(body in ".{0,300}") {
            let m = message(&body);
            prop_assert!(m.available.len() + m.assigned.len() + m.started.len() + m.completed.len() + m.skipped <= body.len().max(1));
        }

        #[test]
        fn no_json_value_is_a_panic(n in any::<i64>(), kind in "[A-Za-z]{0,12}", extra in ".{0,20}") {
            let body = serde_json::json!([{"messageType": kind, "runnerRequestId": n, "x": extra}, n, null, [kind]]).to_string();
            let m = message(&body);
            let _ = (m.available.len(), m.skipped);
        }
    }
}
