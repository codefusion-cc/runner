use std::collections::{BTreeMap, BTreeSet};

use serde_yaml_ng::Value;

use crate::expression::{evaluate, is_pool_expression};
use crate::size::is_size_label;

/// The label these runners serve for a job that takes its runner from a repository variable
/// (`runs-on: ${{ vars.CI_RUNNER || 'ubuntu-latest' }}`) when the variable names no label of theirs yet. CodeFusion
/// Console's runner pool sends a repository's jobs to the Macs by setting the variable to a label of these runners
/// (`self-hosted`, or a size label); until it does, the repository is still served under this label.
pub const POOL_LABEL: &str = "self-hosted";

/// A workflow file bigger than this is not read. GitHub's own limit is far lower; the cap keeps a hostile file from
/// costing the Runner time or memory.
const MAX_FILE_BYTES: usize = 512 * 1024;

/// Labels that name a platform these runners are not: a job asking for one of them is left to other runners.
const FOREIGN: [&str; 6] = ["macos", "osx", "windows", "x64", "x86", "arm"];

/// One `runs-on` request: its labels lowercased, deduplicated and sorted. GitHub compares labels without regard to
/// case, so two spellings of one set are the same set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LabelSet(Vec<String>);

impl LabelSet {
    /// Builds a set from labels: lowercased, trimmed, sorted, without repeats. Empty labels are dropped.
    pub fn new<I, S>(labels: I) -> LabelSet
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let sorted: BTreeSet<String> = labels
            .into_iter()
            .map(|l| l.as_ref().trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect();
        LabelSet(sorted.into_iter().collect())
    }

    /// The labels, sorted.
    pub fn labels(&self) -> &[String] {
        &self.0
    }

    /// The labels joined by commas, unique per set.
    pub fn key(&self) -> String {
        self.0.join(",")
    }

    /// Whether the set has this (lowercase) label.
    pub fn contains(&self, label: &str) -> bool {
        self.0.iter().any(|l| l == label)
    }
}

/// [`self_hosted_with`] without repository variables: every expression over variables stands for [`POOL_LABEL`].
pub fn self_hosted(files: &BTreeMap<String, Vec<u8>>) -> Vec<LabelSet> {
    self_hosted_with(files, &BTreeMap::new())
}

/// Every distinct label set that jobs in the given workflow files request from self-hosted runners these runners
/// can serve: sets that include `self-hosted` or a valid size label (`cf-2g`, `cf-4g`, `cf-6g`, `cf-8g`), name no
/// other platform, and are plain labels or expressions over repository variables, rather than other expressions or
/// runner groups.
///
/// An expression over `vars.` alone (string literals and functions aside) is a job the console's pool may point at
/// these runners. It is evaluated with `vars` when it can be (`||` chains of variables and literals); a value that
/// names a label of these runners stands for that label, any other value, or an expression that cannot be
/// evaluated, for [`POOL_LABEL`]. One that reads anything else (a matrix value, an input) cannot be known ahead of a
/// run and is not served. A call to a reusable workflow counts by its `with: runs-on` input, the runner it passes
/// on. Files that are not valid YAML, or are over 512 KiB, are skipped, since GitHub runs none of their jobs
/// either; a job of a shape that cannot be read is skipped alone. The result is sorted by key and does not depend
/// on the order of files.
pub fn self_hosted_with(files: &BTreeMap<String, Vec<u8>>, vars: &BTreeMap<String, String>) -> Vec<LabelSet> {
    let mut seen = BTreeSet::new();
    for content in files.values() {
        if content.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(doc) = serde_yaml_ng::from_slice::<Value>(content) else {
            continue;
        };
        let Some(jobs) = doc.get("jobs").and_then(Value::as_mapping) else {
            continue;
        };
        for job in jobs.values() {
            if let Some(set) = job.get("runs-on").and_then(|node| label_set(node, vars)) {
                seen.insert(set);
            }
            // The reusable workflow runs in this repository, on the runner its input names.
            let uses = job
                .get("uses")
                .and_then(Value::as_str)
                .is_some_and(|u| !u.is_empty());
            let input = job
                .get("with")
                .and_then(|w| w.get("runs-on"))
                .and_then(Value::as_str);
            if let (true, Some(input)) = (uses, input)
                && is_pool_expression(input)
            {
                seen.insert(LabelSet::new([resolve(input, vars)]));
            }
        }
    }
    seen.into_iter().collect()
}

/// What a pool expression stands for: the label of these runners it evaluates to, else [`POOL_LABEL`].
fn resolve(expression: &str, vars: &BTreeMap<String, String>) -> String {
    evaluate(expression, vars)
        .map(|v| v.trim().to_lowercase())
        .filter(|v| v == POOL_LABEL || is_size_label(v))
        .unwrap_or_else(|| POOL_LABEL.to_string())
}

/// A scalar's text as the YAML file spells it, as far as a label goes.
fn scalar(node: &Value) -> Option<String> {
    match node {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

fn label_set(node: &Value, vars: &BTreeMap<String, String>) -> Option<LabelSet> {
    let labels: Vec<String> = match node {
        Value::Sequence(items) => items.iter().map(scalar).collect::<Option<_>>()?,
        Value::Mapping(map) => {
            // `runs-on: {group: …, labels: …}`: a runner group is an organization feature these runners are not in.
            if map.contains_key("group") {
                return None;
            }
            return label_set(map.get("labels")?, vars);
        }
        other => vec![scalar(other)?],
    };

    let mut set = Vec::with_capacity(labels.len());
    for label in labels {
        let label = if is_pool_expression(&label) {
            resolve(&label, vars)
        } else {
            label
        };
        let label = label.trim().to_lowercase();
        if label.is_empty() || label.contains("${{") || FOREIGN.contains(&label.as_str()) {
            return None;
        }
        set.push(label);
    }
    let set = LabelSet::new(set);
    (set.contains(POOL_LABEL) || set.labels().iter().any(|l| is_size_label(l))).then_some(set)
}
