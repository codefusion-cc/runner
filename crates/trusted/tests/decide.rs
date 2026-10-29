//! What a trusted host lets run, from the threat: a pull request (or anything else but main's own deploy, release and
//! signing runs) asking for the label `trusted`. Every case is a job message as the Actions runner's Worker gets it.

use cf_trusted::{DEFAULT_ALLOW, EVENTS, MAX_WORKFLOWS, Policy, decide, is_repository, read_run};
use proptest::prelude::*;
use serde_json::{Value, json};

const REPO: &str = "codefusion-cc/codefusion";

fn workflow_ref(file: &str) -> String {
    format!("{REPO}/.github/workflows/{file}@refs/heads/main")
}

/// A job message whose `contextData.github` holds `fields`, written the way the Actions runner writes context data.
fn message_with(fields: &[(&str, Value)]) -> String {
    let d: Vec<Value> = fields.iter().map(|(k, v)| json!({ "k": k, "v": v })).collect();
    json!({
        "messageType": "PipelineAgentJobRequest",
        "plan": { "planId": "1" },
        "contextData": {
            "github": { "t": 2, "d": d },
            "needs": { "t": 2 }
        },
        "variables": { "system.github.token": { "value": "***", "isSecret": true } },
        "steps": []
    })
    .to_string()
}

fn run(repository: &str, git_ref: &str, workflow: &str, event: &str, attempt: &str) -> String {
    message_with(&[
        ("repository", json!(repository)),
        ("ref", json!(git_ref)),
        ("workflow_ref", json!(workflow)),
        ("event_name", json!(event)),
        ("run_attempt", json!(attempt)),
        ("sha", json!("0123456789abcdef0123456789abcdef01234567")),
    ])
}

fn main_run(file: &str, event: &str) -> String {
    run(REPO, "refs/heads/main", &workflow_ref(file), event, "1")
}

fn refused(message: &str) -> String {
    decide(&Policy::defaults(), message).expect_err("refused")
}

#[test]
fn mains_production_deploy_by_hand_runs() {
    let accepted = decide(
        &Policy::defaults(),
        &main_run("deploy-production.yml", "workflow_dispatch"),
    )
    .expect("accepted");
    assert_eq!(accepted.workflow, "deploy-production.yml");
    assert_eq!(accepted.event, "workflow_dispatch");
    // What the machine's log says of it.
    assert_eq!(accepted.to_string(), "deploy-production.yml on workflow_dispatch");
}

#[test]
fn the_console_makes_and_removes_machines_on_the_trusted_host_when_it_dispatches_them_only() {
    let defaults = Policy::defaults();
    assert!(decide(&defaults, &main_run("machines.yml", "workflow_dispatch")).is_ok());
    for event in ["push", "schedule"] {
        assert!(
            decide(&defaults, &main_run("machines.yml", event)).is_err(),
            "{event}"
        );
    }
}

#[test]
fn every_default_workflow_runs_on_its_own_events_only() {
    for (file, events) in DEFAULT_ALLOW {
        for event in EVENTS {
            let verdict = decide(&Policy::defaults(), &main_run(file, event));
            assert_eq!(
                verdict.is_ok(),
                events.contains(&event),
                "{file} on {event}: {verdict:?}"
            );
        }
    }
}

#[test]
fn a_pull_request_is_refused() {
    let message = run(
        REPO,
        "refs/pull/41/merge",
        &format!("{REPO}/.github/workflows/deploy-production.yml@refs/pull/41/merge"),
        "pull_request",
        "1",
    );
    let why = refused(&message);
    assert!(why.contains("refs/pull/41/merge"), "{why}");
    // Even a pull request that claims main's ref and workflow is refused for its event.
    let why = refused(&main_run("deploy-production.yml", "pull_request"));
    assert!(why.contains("\"pull_request\""), "{why}");
}

#[test]
fn pull_request_target_is_refused_though_it_runs_on_main() {
    // pull_request_target runs the base branch's workflow, so its ref and workflow are main's.
    let why = refused(&main_run("release.yml", "pull_request_target"));
    assert!(why.contains("pull_request_target"), "{why}");
}

#[test]
fn events_a_pull_request_or_another_run_can_start_are_refused_everywhere() {
    for event in [
        "issue_comment",
        "merge_group",
        "workflow_run",
        "pull_request_review",
        "repository_dispatch",
        "",
    ] {
        for (file, _) in DEFAULT_ALLOW {
            assert!(
                decide(&Policy::defaults(), &main_run(file, event)).is_err(),
                "{file} on {event}"
            );
        }
    }
}

#[test]
fn a_dispatch_on_another_branch_is_refused() {
    for branch in ["refs/heads/feat/deploy", "refs/heads/main-2", "refs/heads/mainx"] {
        let message = run(
            REPO,
            branch,
            &format!("{REPO}/.github/workflows/deploy-production.yml@{branch}"),
            "workflow_dispatch",
            "1",
        );
        let why = refused(&message);
        assert!(why.contains(branch), "{why}");
    }
}

#[test]
fn refs_that_only_look_like_main_are_refused() {
    for look_alike in [
        "refs/heads/Main",
        "refs/tags/main",
        "main",
        "refs/heads/main ",
        "refs/heads/main/x",
        "refs/remotes/origin/main",
    ] {
        // The workflow's own ref is main's; the run's is not.
        let message = run(
            REPO,
            look_alike,
            &workflow_ref("deploy-production.yml"),
            "workflow_dispatch",
            "1",
        );
        assert!(
            refused(&message).contains("this machine runs main's"),
            "{look_alike}"
        );
        // And the other way round.
        let message = run(
            REPO,
            "refs/heads/main",
            &format!("{REPO}/.github/workflows/deploy-production.yml@{look_alike}"),
            "workflow_dispatch",
            "1",
        );
        assert!(
            refused(&message).contains("not a workflow file of"),
            "{look_alike}"
        );
    }
}

#[test]
fn a_re_run_is_refused() {
    for attempt in [
        json!("2"),
        json!("0"),
        json!(""),
        json!("01"),
        json!(" 1"),
        json!(2),
        json!(2.0),
    ] {
        let message = message_with(&[
            ("repository", json!(REPO)),
            ("ref", json!("refs/heads/main")),
            ("workflow_ref", json!(workflow_ref("deploy-production.yml"))),
            ("event_name", json!("workflow_dispatch")),
            ("run_attempt", attempt.clone()),
        ]);
        let why = refused(&message);
        assert!(why.contains("re-run"), "{attempt}: {why}");
    }
    // A number that is not whole is no attempt at all, though it rounds to the first.
    for odd in [json!(1.4), json!(0.6), json!(-1), json!(1e300)] {
        let message = message_with(&[
            ("repository", json!(REPO)),
            ("ref", json!("refs/heads/main")),
            ("workflow_ref", json!(workflow_ref("deploy-production.yml"))),
            ("event_name", json!("workflow_dispatch")),
            ("run_attempt", odd.clone()),
        ]);
        let why = refused(&message);
        assert!(why.contains("not a whole number"), "{odd}: {why}");
    }
    // The Actions runner writes a number as 1.0: still the first attempt.
    for first in [json!(1), json!(1.0)] {
        let message = message_with(&[
            ("repository", json!(REPO)),
            ("ref", json!("refs/heads/main")),
            ("workflow_ref", json!(workflow_ref("deploy-production.yml"))),
            ("event_name", json!("workflow_dispatch")),
            ("run_attempt", first.clone()),
        ]);
        assert!(decide(&Policy::defaults(), &message).is_ok(), "{first}");
    }
}

#[test]
fn a_workflow_not_on_the_list_is_refused() {
    for file in [
        "ci.yml",
        "deploy.yml",
        "deploy-production.yaml",
        "Deploy-production.yml",
        "automerge.yml",
    ] {
        let why = refused(&main_run(file, "push"));
        assert!(why.contains("not on this machine's allow-list"), "{file}: {why}");
    }
}

#[test]
fn a_workflow_of_another_repository_or_a_fork_is_refused() {
    let elsewhere = [
        "someone/codefusion",
        "codefusion-cc/codefusion-console",
        "codefusion-cc/codefusion2",
        "codefusion-cc/Codefusion",
        "codefusion-cc",
    ];
    for repo in elsewhere {
        // A fork's (or another repository's) workflow, run in the fork.
        let message = run(
            repo,
            "refs/heads/main",
            &format!("{repo}/.github/workflows/deploy-production.yml@refs/heads/main"),
            "workflow_dispatch",
            "1",
        );
        let why = refused(&message);
        assert!(
            why.contains("this machine runs jobs of codefusion-cc/codefusion only"),
            "{repo}: {why}"
        );
        // Another repository's workflow under this repository's name: a call into a workflow elsewhere.
        let message = run(
            REPO,
            "refs/heads/main",
            &format!("{repo}/.github/workflows/deploy-production.yml@refs/heads/main"),
            "workflow_dispatch",
            "1",
        );
        assert!(refused(&message).contains("not a workflow file of"), "{repo}");
    }
    // A path that walks out of .github/workflows.
    for sneaky in [
        format!("{REPO}/.github/workflows/../workflows/deploy-production.yml@refs/heads/main"),
        format!("{REPO}/.github/workflows/sub/deploy-production.yml@refs/heads/main"),
        format!("{REPO}/.github/workflows/deploy-production.yml@refs/heads/main@refs/heads/main"),
        format!("x/{REPO}/.github/workflows/deploy-production.yml@refs/heads/main"),
    ] {
        let message = run(REPO, "refs/heads/main", &sneaky, "workflow_dispatch", "1");
        assert!(refused(&message).contains("not a workflow file of"), "{sneaky}");
    }
}

#[test]
fn a_missing_fact_is_refused() {
    let all = [
        ("repository", json!(REPO)),
        ("ref", json!("refs/heads/main")),
        ("workflow_ref", json!(workflow_ref("deploy-production.yml"))),
        ("event_name", json!("workflow_dispatch")),
        ("run_attempt", json!("1")),
    ];
    assert!(decide(&Policy::defaults(), &message_with(&all)).is_ok());
    for left_out in 0..all.len() {
        let some: Vec<(&str, Value)> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != left_out)
            .map(|(_, field)| field.clone())
            .collect();
        let why = refused(&message_with(&some));
        assert!(
            why.contains(&format!("github.{} is missing", all[left_out].0)),
            "{why}"
        );
        // Null, a number or a list in its place is no better.
        for wrong in [
            Value::Null,
            json!(["refs/heads/main"]),
            json!({ "t": 0, "s": "x" }),
            json!(true),
        ] {
            let mut replaced = all.to_vec();
            replaced[left_out].1 = wrong.clone();
            assert!(
                decide(&Policy::defaults(), &message_with(&replaced)).is_err(),
                "{wrong}"
            );
        }
    }
}

#[test]
fn a_fact_given_twice_is_refused_in_any_case() {
    let main = message_with(&[
        ("repository", json!(REPO)),
        ("ref", json!("refs/heads/main")),
        ("Ref", json!("refs/pull/1/merge")),
        ("workflow_ref", json!(workflow_ref("deploy-production.yml"))),
        ("event_name", json!("workflow_dispatch")),
        ("run_attempt", json!("1")),
    ]);
    assert!(refused(&main).contains("more than once"));
    // Two github contexts, or two contextData: whichever a reader takes, the message is refused.
    let good = main_run("deploy-production.yml", "workflow_dispatch");
    let twice = good.replacen("\"contextData\"", "\"contextData\":{},\"ContextData\"", 1);
    assert!(refused(&twice).contains("more than once"), "{twice}");
    let github = good.replacen("\"github\"", "\"GitHub\":{\"t\":2},\"github\"", 1);
    assert!(refused(&github).contains("more than once"), "{github}");
}

/// The Worker's dictionaries ignore case as .NET's `OrdinalIgnoreCase` does, which takes `ſ` (long s) for `s` and
/// `ı` (dotless i) for `i`: a second fact spelled so is the same name to the Worker, and the message is refused.
#[test]
fn a_fact_given_twice_in_a_spelling_only_unicode_case_folds_together_is_refused() {
    let fork = "someone/codefusion";
    let repository = message_with(&[
        ("repository", json!(REPO)),
        ("repo\u{17f}itory", json!(fork)),
        ("ref", json!("refs/heads/main")),
        ("workflow_ref", json!(workflow_ref("deploy-production.yml"))),
        ("event_name", json!("workflow_dispatch")),
        ("run_attempt", json!("1")),
    ]);
    assert!(refused(&repository).contains("more than once"), "{repository}");
    let good = main_run("deploy-production.yml", "workflow_dispatch");
    let github = good.replacen("\"github\"", "\"g\u{131}thub\":{\"t\":2},\"github\"", 1);
    assert!(refused(&github).contains("more than once"), "{github}");
}

#[test]
fn a_message_that_is_not_a_job_message_is_refused() {
    for text in [
        "",
        "null",
        "[]",
        "{}",
        "{\"contextData\":null}",
        "{\"contextData\":{\"github\":\"push\"}}",
        "{\"contextData\":{\"github\":{\"t\":5,\"d\":[]}}}",
        "{\"contextData\":{\"github\":{\"d\":[]}}}",
        "{\"contextData\":{\"github\":{\"t\":2,\"d\":{}}}}",
        "{\"contextData\":{\"github\":{\"t\":2,\"d\":[{\"k\":\"ref\"}]}}}",
        "{\"contextData\":{\"github\":{\"t\":2,\"d\":[{\"k\":1,\"v\":\"x\"}]}}}",
        "{\"contextData\":{\"github\":{\"t\":2,\"d\":[{\"k\":\"ref\",\"v\":\"x\",\"w\":1}]}}}",
    ] {
        assert!(decide(&Policy::defaults(), text).is_err(), "{text}");
    }
    // Every fact there, in something that is not the Actions runner's dictionary (`t` 2).
    let facts: Vec<Value> = [
        ("repository", REPO.to_string()),
        ("ref", "refs/heads/main".to_string()),
        ("workflow_ref", workflow_ref("deploy-production.yml")),
        ("event_name", "workflow_dispatch".to_string()),
        ("run_attempt", "1".to_string()),
    ]
    .iter()
    .map(|(k, v)| json!({ "k": k, "v": v }))
    .collect();
    for github in [
        json!({ "t": 5, "d": facts }),
        json!({ "t": "2", "d": facts }),
        json!({ "d": facts }),
    ] {
        let message = json!({ "contextData": { "github": github } }).to_string();
        let why = decide(&Policy::defaults(), &message).expect_err(&message);
        assert!(why.contains("not a dictionary"), "{github}: {why}");
    }
    let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    assert!(decide(&Policy::defaults(), &deep).is_err());
}

#[test]
fn an_empty_or_malformed_allow_list_refuses_everything() {
    let good = main_run("deploy-production.yml", "workflow_dispatch");
    let broken = [
        json!(null),
        json!([]),
        json!("deploy-production.yml"),
        json!({}),
        json!({ "repository": REPO }),
        json!({ "repository": REPO, "allow": {} }),
        json!({ "repository": REPO, "allow": [] }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": [] } }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": "workflow_dispatch" } }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": ["workflow_dispatch", "pull_request"] } }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": ["workflow_dispatch"], "ci.yml": ["pull_request_target"] } }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": ["workflow_dispatch"], "x.yml": ["issue_comment"] } }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": ["workflow_dispatch"], "x.yml": ["merge_group"] } }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": ["workflow_dispatch"], "x.yml": ["workflow_run"] } }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": [1] } }),
        json!({ "repository": REPO, "allow": { ".github/workflows/deploy-production.yml": ["workflow_dispatch"] } }),
        json!({ "repository": REPO, "allow": { "deploy-production": ["workflow_dispatch"] } }),
        json!({ "repository": REPO, "allow": { "../deploy-production.yml": ["workflow_dispatch"] } }),
        json!({ "repository": "codefusion", "allow": { "deploy-production.yml": ["workflow_dispatch"] } }),
        json!({ "repository": 7, "allow": { "deploy-production.yml": ["workflow_dispatch"] } }),
        json!({ "repository": REPO, "alow": { "deploy-production.yml": ["workflow_dispatch"] } }),
        json!({ "repository": REPO, "allow": { "deploy-production.yml": ["workflow_dispatch"] }, "events": "all" }),
    ];
    for policy in broken {
        let verdict = decide(&Policy::new(policy.clone()), &good);
        let why = verdict.expect_err(&policy.to_string());
        assert!(why.contains("refuses every job"), "{policy}: {why}");
    }
    // A list of the most workflows holds; one more refuses everything.
    let mut most: serde_json::Map<String, Value> = (1..MAX_WORKFLOWS)
        .map(|i| (format!("w{i}.yml"), json!(["push"])))
        .collect();
    most.insert("deploy-production.yml".into(), json!(["workflow_dispatch"]));
    assert!(decide(&Policy::new(json!({ "allow": most.clone() })), &good).is_ok());
    most.insert("w0.yml".into(), json!(["push"]));
    let why = decide(&Policy::new(json!({ "allow": most })), &good).expect_err("one too many");
    assert!(
        why.contains(&format!("names {} workflows", MAX_WORKFLOWS + 1)),
        "{why}"
    );
    // The refusal names what does not hold.
    let why = decide(
        &Policy::new(
            json!({ "allow": { "deploy-production.yml": ["workflow_dispatch", {"on": "pull_request"}] } }),
        ),
        &good,
    )
    .expect_err("refused");
    assert!(why.contains(r#"{\"on\":\"pull_request\"}"#), "{why}");
    let why = decide(
        &Policy::new(json!({ "repository": ["org/app"], "allow": {} })),
        &good,
    )
    .expect_err("refused");
    assert!(why.contains(r#"[\"org/app\"]"#), "{why}");
}

#[test]
fn a_repository_is_owner_and_name_of_the_characters_github_allows() {
    let long = "a".repeat(100);
    for name in ["codefusion-cc/codefusion", "A.b_c-1/x", &format!("{long}/{long}")] {
        assert!(is_repository(name), "{name}");
    }
    let too_long = "a".repeat(101);
    for name in [
        "",
        "/",
        "/codefusion",
        "codefusion-cc/",
        "codefusion",
        "codefusion-cc/codefusion/x",
        "codefusion cc/codefusion",
        "codefusion-cc/code\nfusion",
        "codefusion-cc/codefusiön",
        "./codefusion",
        "codefusion-cc/..",
        &format!("{too_long}/x"),
        &format!("x/{too_long}"),
    ] {
        assert!(!is_repository(name), "{name}");
    }
}

#[test]
fn a_written_allow_list_is_used_as_written() {
    // Left out, the repository is codefusion's.
    let policy = Policy::new(json!({ "allow": { "deploy-production.yml": ["workflow_dispatch"] } }));
    assert!(decide(&policy, &main_run("deploy-production.yml", "workflow_dispatch")).is_ok());
    assert!(decide(&policy, &main_run("release.yml", "push")).is_err());
    let other =
        Policy::new(json!({ "repository": "org/shop", "allow": { "sign.yaml": ["push", "schedule"] } }));
    let message = run(
        "org/shop",
        "refs/heads/main",
        "org/shop/.github/workflows/sign.yaml@refs/heads/main",
        "schedule",
        "1",
    );
    assert!(decide(&other, &message).is_ok());
    assert!(decide(&other, &main_run("deploy-production.yml", "workflow_dispatch")).is_err());
}

#[test]
fn a_refusal_says_every_reason_and_quotes_what_it_was_sent() {
    let message = run(
        "evil/fork",
        "refs/pull/9/merge\n::error::forged",
        "evil/fork/.github/workflows/ci.yml@refs/pull/9/merge",
        "pull_request",
        "3",
    );
    let why = refused(&message);
    for part in [
        "evil/fork",
        "refs/pull/9/merge",
        "re-run",
        "not a workflow file of",
    ] {
        assert!(why.contains(part), "{part}: {why}");
    }
    assert!(!why.contains('\n'), "{why}");
    let long = "x".repeat(5000);
    let why = refused(&run(REPO, &long, &long, &long, &long));
    assert!(why.len() < 1500, "{}", why.len());
    assert!(why.contains(&format!("\"{}\"...", "x".repeat(120))), "{why}");
    // A value of exactly the quoted length is whole, with nothing cut.
    let whole = "y".repeat(120);
    let why = refused(&run(
        REPO,
        &whole,
        &workflow_ref("deploy-production.yml"),
        "workflow_dispatch",
        "1",
    ));
    assert!(why.contains(&format!("\"{whole}\", and")), "{why}");
}

#[test]
fn read_run_reads_the_five_facts() {
    let run = read_run(&main_run("release.yml", "push")).expect("read");
    assert_eq!(run.repository, REPO);
    assert_eq!(run.git_ref, "refs/heads/main");
    assert_eq!(run.workflow_ref, workflow_ref("release.yml"));
    assert_eq!(run.event_name, "push");
    assert_eq!(run.run_attempt, "1");
}

fn fact() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(REPO.to_string()),
        Just("refs/heads/main".to_string()),
        Just("1".to_string()),
        Just("workflow_dispatch".to_string()),
        Just("push".to_string()),
        Just("pull_request".to_string()),
        Just("schedule".to_string()),
        Just(workflow_ref("deploy-production.yml")),
        Just(workflow_ref("release.yml")),
        Just(workflow_ref("ci.yml")),
        "[ -~]{0,40}",
        "\\PC{0,20}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// Whatever the context says, nothing is accepted unless it is exactly main's first attempt of an allowed
    /// workflow for one of its events in codefusion.
    #[test]
    fn nothing_off_the_list_is_accepted(
        repository in fact(),
        git_ref in fact(),
        workflow in fact(),
        event in fact(),
        attempt in fact(),
    ) {
        let message = run(&repository, &git_ref, &workflow, &event, &attempt);
        let on_list = DEFAULT_ALLOW.iter().any(|(file, events)| {
            workflow == workflow_ref(file) && events.contains(&event.as_str())
        });
        let expected = repository == REPO && git_ref == "refs/heads/main" && attempt == "1" && on_list;
        prop_assert_eq!(decide(&Policy::defaults(), &message).is_ok(), expected);
    }

    /// Bytes that are not a job message never pass.
    #[test]
    fn noise_is_refused(text in "\\PC{0,200}") {
        prop_assert!(decide(&Policy::defaults(), &text).is_err());
    }
}
