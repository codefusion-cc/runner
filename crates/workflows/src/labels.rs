use std::collections::{BTreeMap, BTreeSet};

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::expression::{evaluate, is_pool_expression};
use crate::size::is_size_label;

/// The label these Actions runners serve for a job that takes its Actions runner from a repository variable (`runs-on:
/// ${{ vars.CI_RUNNER || 'ubuntu-latest' }}`) when the variable names no label of theirs yet. CodeFusion Console's
/// Actions runner pool sends a repository's jobs to the Macs by setting the variable to a label of these Actions
/// runners (`self-hosted`, or a size label); until it does, the repository is still served under this label.
pub const POOL_LABEL: &str = "self-hosted";

/// The label of a job that runs Docker: it gets a Docker daemon of its own beside its container, in a machine of
/// the size its other labels name (4 GB with none). Served only for the repositories a Runner is told to give
/// Docker; a variable may name it (`CI_RUNNER_DOCKER=cf-docker`), like a size label.
pub const DOCKER_LABEL: &str = "cf-docker";

/// A workflow file bigger than this is not read, and whoever fetches the files need not fetch it. GitHub's own limit
/// is far lower; the cap keeps a hostile file from costing the Runner time or memory.
pub const MAX_FILE_BYTES: usize = 512 * 1024;

/// Labels that name a platform these Actions runners are not: a job asking for one of them is left to other Actions
/// runners.
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

    /// Whether the set asks for Docker ([`DOCKER_LABEL`]).
    pub fn docker(&self) -> bool {
        self.contains(DOCKER_LABEL)
    }

    /// The host labels the set asks for ([`is_host_label`]): only a host that has every one of them serves it.
    pub fn host_labels(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .map(String::as_str)
            .filter(|label| is_host_label(label))
    }
}

/// What [`is_host_label`] takes, for a person who wrote another label.
pub const HOST_LABEL_RULE: &str = "`cf-` and a name of lowercase letters, digits and single dashes that starts with a \
     letter, 40 characters at most, and no label jobs ask for by itself, such as a size or cf-docker";

/// Whether a (lowercase) label names hosts rather than a machine: `cf-` and a name of lowercase letters, digits and
/// single dashes that starts with a letter, 40 characters in all at most, and no label these Actions runners serve
/// (a size, [`DOCKER_LABEL`]): `cf-macos`, `cf-linux`. A host serves a set with host labels only when its config
/// lists every one of them, so a job asks for a kind of host with `[cf-8g, cf-macos]`. A host label alone names no
/// size.
pub fn is_host_label(label: &str) -> bool {
    // HOST_LABEL_RULE says this in words.
    let Some(name) = label.strip_prefix("cf-") else {
        return false;
    };
    label.len() <= 40
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        // Never a label these Actions runners serve themselves (a size, cf-docker), whatever is added to those.
        && !is_ours(label)
}

/// [`self_hosted_with`] without repository variables: every expression over variables stands for [`POOL_LABEL`].
pub fn self_hosted(files: &BTreeMap<String, Vec<u8>>) -> Vec<LabelSet> {
    self_hosted_with(files, &BTreeMap::new())
}

/// Every distinct label set that jobs in the given workflow files request from self-hosted Actions runners these
/// Actions runners can serve: sets that include `self-hosted`, a valid size label (`cf-2g`, `cf-4g`, `cf-6g`,
/// `cf-8g`) or [`DOCKER_LABEL`], name no other platform, and are plain labels or expressions over repository variables, rather than other
/// expressions or Actions runner groups.
///
/// An expression over `vars.` alone (string literals and functions aside) is a job the console's pool may point at
/// these Actions runners. It is evaluated with `vars` when it can be (`||` chains of variables and literals); a value
/// that names a label of these Actions runners stands for that label, any other value, or an expression that cannot be
/// evaluated, for [`POOL_LABEL`]. One that reads anything else (a matrix value, an input) cannot be known ahead of a
/// run and is not served. A call to a reusable workflow counts by its `with: runs-on` input, the Actions runner it
/// passes on. Files that are not valid YAML, or are over 512 KiB, are skipped, since GitHub runs none of their jobs
/// either; a job of a shape that cannot be read is skipped alone. The result is sorted by key and does not depend on
/// the order of files.
pub fn self_hosted_with(files: &BTreeMap<String, Vec<u8>>, vars: &BTreeMap<String, String>) -> Vec<LabelSet> {
    let mut seen = BTreeSet::new();
    for content in files.values() {
        if content.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(doc) = serde_saphyr::from_slice::<Node>(content) else {
            continue;
        };
        let Some(Node::Mapping(jobs)) = doc.get("jobs") else {
            continue;
        };
        for job in jobs.values() {
            if let Some(set) = job.get("runs-on").and_then(|node| label_set(node, vars)) {
                seen.insert(set);
            }
            // The reusable workflow runs in this repository, on the Actions runner its input names.
            let uses = job
                .get("uses")
                .and_then(Node::as_str)
                .is_some_and(|u| !u.is_empty());
            let input = job
                .get("with")
                .and_then(|w| w.get("runs-on"))
                .and_then(Node::as_str);
            if let (true, Some(input)) = (uses, input)
                && is_pool_expression(input)
            {
                seen.insert(LabelSet::new(resolve(input, vars)));
            }
        }
    }
    seen.into_iter().collect()
}

/// What a pool expression stands for: the labels it evaluates to when one of them is a label of these Actions
/// runners, else [`POOL_LABEL`].
fn resolve(expression: &str, vars: &BTreeMap<String, String>) -> Vec<String> {
    evaluate(expression, vars)
        .map(|labels| {
            labels
                .iter()
                .map(|label| label.trim().to_lowercase())
                .filter(|label| !label.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|labels| labels.iter().any(|label| is_ours(label)))
        .unwrap_or_else(|| vec![POOL_LABEL.to_string()])
}

/// A node of a workflow file, as far as labels go: a scalar keeps the text YAML gives it, and a mapping's keys are
/// their text too, so that `1:` and `true:` are keys like any other.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Null,
    Scalar(String),
    Sequence(Vec<Node>),
    Mapping(BTreeMap<String, Node>),
}

impl Node {
    fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Mapping(map) => map.get(key),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) => Some(s),
            _ => None,
        }
    }

    /// A scalar's text, as far as a label goes; a key's text.
    fn text(&self) -> Option<String> {
        match self {
            Node::Scalar(s) => Some(s.clone()),
            Node::Null => Some(String::new()),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(NodeVisitor)
    }
}

struct NodeVisitor;

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = Node;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any YAML node")
    }

    fn visit_unit<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_none<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Node, D::Error> {
        Node::deserialize(deserializer)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Node, E> {
        Ok(Node::Scalar(v.to_string()))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Node, E> {
        Ok(Node::Scalar(v.to_string()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Node, E> {
        Ok(Node::Scalar(v.to_string()))
    }

    fn visit_i128<E>(self, v: i128) -> Result<Node, E> {
        Ok(Node::Scalar(v.to_string()))
    }

    fn visit_u128<E>(self, v: u128) -> Result<Node, E> {
        Ok(Node::Scalar(v.to_string()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Node, E> {
        Ok(Node::Scalar(v.to_string()))
    }

    fn visit_str<E>(self, v: &str) -> Result<Node, E> {
        Ok(Node::Scalar(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Node, E> {
        Ok(Node::Scalar(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Node::Sequence(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        let mut nodes = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<Node, Node>()? {
            // A key that is itself a sequence or a mapping names nothing a workflow reads.
            if let Some(key) = key.text() {
                nodes.insert(key, value);
            }
        }
        Ok(Node::Mapping(nodes))
    }
}

fn label_set(node: &Node, vars: &BTreeMap<String, String>) -> Option<LabelSet> {
    let labels: Vec<String> = match node {
        Node::Sequence(items) => items.iter().map(Node::text).collect::<Option<_>>()?,
        Node::Mapping(map) => {
            // `runs-on: {group: …, labels: …}`: an Actions runner group is an organization feature these Actions
            // runners are not in.
            if map.contains_key("group") {
                return None;
            }
            return label_set(map.get("labels")?, vars);
        }
        other => vec![other.text()?],
    };

    let mut set = Vec::with_capacity(labels.len());
    for label in labels {
        let labels = if is_pool_expression(&label) {
            resolve(&label, vars)
        } else {
            vec![label]
        };
        for label in labels {
            let label = label.trim().to_lowercase();
            if label.is_empty() || label.contains("${{") || FOREIGN.contains(&label.as_str()) {
                return None;
            }
            set.push(label);
        }
    }
    let set = LabelSet::new(set);
    set.labels().iter().any(|l| is_ours(l)).then_some(set)
}

/// Whether a (lowercase) label is one these Actions runners serve: [`POOL_LABEL`], [`DOCKER_LABEL`] or a size.
fn is_ours(label: &str) -> bool {
    label == POOL_LABEL || label == DOCKER_LABEL || is_size_label(label)
}
