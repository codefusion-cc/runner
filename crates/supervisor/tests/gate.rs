//! The trusted gate as the Actions runner's listener meets it: the first message down the Worker's channel, framed
//! the way the listener frames it, and what the gate makes of it before any step of the job could run.

use cf_supervisor::gate::{
    self, GateConfig, MAX_MESSAGE_BYTES, NEW_JOB_REQUEST, VERDICT_MAX, read_back, read_request, refusal,
    verdict_line,
};
use cf_trusted::Policy;
use serde_json::json;
use std::io::{Cursor, Read};

const REPO: &str = "codefusion-cc/codefusion";

/// A job message with GitHub's facts, as the Actions runner writes its context data.
fn message(git_ref: &str, file: &str, event: &str, attempt: &str) -> String {
    let fields = [
        ("repository", REPO.to_string()),
        ("ref", git_ref.to_string()),
        (
            "workflow_ref",
            format!("{REPO}/.github/workflows/{file}@{git_ref}"),
        ),
        ("event_name", event.to_string()),
        ("run_attempt", attempt.to_string()),
    ];
    let d: Vec<_> = fields.iter().map(|(k, v)| json!({ "k": k, "v": v })).collect();
    json!({ "contextData": { "github": { "t": 2, "d": d } }, "steps": [] }).to_string()
}

/// The bytes of one message on the channel: type, length in bytes, UTF-16LE text.
fn frame(kind: i32, body: &[u8]) -> Vec<u8> {
    let mut bytes = kind.to_le_bytes().to_vec();
    bytes.extend_from_slice(&(body.len() as i32).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn job_frame(text: &str) -> Vec<u8> {
    frame(NEW_JOB_REQUEST, &utf16(text))
}

fn verdict(bytes: &[u8]) -> Result<String, String> {
    let request = read_request(&mut Cursor::new(bytes))?;
    gate::decide(&Policy::defaults(), &request).map(|accepted| accepted.to_string())
}

#[test]
fn mains_deploy_is_handed_on_byte_for_byte_and_nothing_after_it_is_taken() {
    let job = job_frame(&message(
        "refs/heads/main",
        "deploy-production.yml",
        "workflow_dispatch",
        "1",
    ));
    // A cancel the listener sends later must reach the Worker, not the gate.
    let later = frame(2, &utf16("{}"));
    let mut channel = Cursor::new([job.clone(), later.clone()].concat());
    let request = read_request(&mut channel).expect("read");
    assert_eq!(request.frame, job);
    assert_eq!(
        gate::decide(&Policy::defaults(), &request).map(|accepted| accepted.to_string()),
        Ok("deploy-production.yml on workflow_dispatch".to_string())
    );
    let mut rest = Vec::new();
    channel.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, later);
}

#[test]
fn a_pull_request_job_is_refused_before_the_worker() {
    let why = verdict(&job_frame(&message(
        "refs/pull/7/merge",
        "deploy-production.yml",
        "pull_request",
        "1",
    )))
    .expect_err("refused");
    assert!(why.contains("the run is on \"refs/pull/7/merge\""), "{why}");
    assert!(why.contains("not a workflow file of"), "{why}");
}

#[test]
fn a_re_run_of_mains_deploy_is_refused() {
    let why = verdict(&job_frame(&message(
        "refs/heads/main",
        "deploy-production.yml",
        "workflow_dispatch",
        "2",
    )))
    .expect_err("refused");
    assert!(why.contains("\"2\""), "{why}");
}

#[test]
fn a_first_message_that_is_not_a_new_job_is_refused() {
    let text = message(
        "refs/heads/main",
        "deploy-production.yml",
        "workflow_dispatch",
        "1",
    );
    for kind in [0, 2, 3, 4, 5, -1, i32::MAX] {
        let why = verdict(&frame(kind, &utf16(&text))).expect_err("refused");
        assert!(why.contains(&format!("type {kind}")), "{kind}: {why}");
    }
}

#[test]
fn text_that_is_not_utf16_is_refused() {
    let mut odd = utf16(&message(
        "refs/heads/main",
        "deploy-production.yml",
        "workflow_dispatch",
        "1",
    ));
    odd.push(b' ');
    let why = verdict(&frame(NEW_JOB_REQUEST, &odd)).expect_err("odd length");
    assert!(why.contains("odd number of bytes"), "{why}");
    // A high surrogate with nothing after it.
    let lone = [utf16("{\"a\":\""), vec![0x00, 0xD8], utf16("\"}")].concat();
    let why = verdict(&frame(NEW_JOB_REQUEST, &lone)).expect_err("lone surrogate");
    assert!(why.contains("surrogate"), "{why}");
}

#[test]
fn a_channel_that_ends_early_or_announces_too_much_is_refused() {
    assert!(verdict(&[]).unwrap_err().contains("did not come"));
    assert!(verdict(&[1, 0, 0, 0, 10]).unwrap_err().contains("did not come"));
    let mut cut = job_frame(&message(
        "refs/heads/main",
        "deploy-production.yml",
        "workflow_dispatch",
        "1",
    ));
    let whole = cut.len() - 8;
    cut.truncate(cut.len() - 3);
    let why = verdict(&cut).unwrap_err();
    assert!(
        why.contains(&format!("after {} of {whole} bytes", whole - 3)),
        "{why}"
    );
    for length in [-1, i32::MIN, MAX_MESSAGE_BYTES as i32 + 1, i32::MAX] {
        let mut header = NEW_JOB_REQUEST.to_le_bytes().to_vec();
        header.extend_from_slice(&length.to_le_bytes());
        let why = verdict(&header).unwrap_err();
        assert!(
            why.contains(&format!("announces {length} bytes")),
            "{length}: {why}"
        );
    }
}

#[test]
fn the_largest_message_the_listener_sends_is_read() {
    let mut header = NEW_JOB_REQUEST.to_le_bytes().to_vec();
    header.extend_from_slice(&(MAX_MESSAGE_BYTES as i32).to_le_bytes());
    let mut channel = Cursor::new(header).chain(std::io::repeat(0x20).take(MAX_MESSAGE_BYTES as u64));
    let request = read_request(&mut channel).expect("the largest message reads");
    assert_eq!(request.frame.len(), 8 + MAX_MESSAGE_BYTES);
    assert!(gate::decide(&Policy::defaults(), &request).is_err());
}

#[test]
fn an_empty_allow_list_in_the_gates_file_refuses_mains_deploy() {
    let config: GateConfig = serde_json::from_value(json!({
        "policy": { "repository": REPO, "allow": {} },
        "worker": "/home/runner/bin/Runner.Worker.real",
        "verdict": "/home/runner/.codefusion-gate"
    }))
    .unwrap();
    let request = read_request(&mut Cursor::new(job_frame(&message(
        "refs/heads/main",
        "deploy-production.yml",
        "workflow_dispatch",
        "1",
    ))))
    .unwrap();
    let why = gate::decide(&config.policy, &request).expect_err("refused");
    assert!(why.contains("refuses every job"), "{why}");
}

#[test]
fn a_refusal_names_the_machine_and_the_reason() {
    assert_eq!(
        refusal("the ref is \"refs/heads/dev\""),
        "codefusion-runner: this trusted machine runs only main's deploy, release and signing jobs, and refuses \
         this one before any step: the ref is \"refs/heads/dev\"."
    );
}

#[test]
fn a_verdict_is_one_bounded_line() {
    let line = verdict_line(&Err(format!("one\nline {}", "é".repeat(VERDICT_MAX))));
    assert!(line.starts_with("refused: one line éé"), "{line}");
    assert!(!line.contains('\n'));
    assert!(
        line.len() <= VERDICT_MAX && line.len() > VERDICT_MAX - 2,
        "{}",
        line.len()
    );
    // What the supervisor reads back may have been written by the job: its first line, no control characters.
    assert_eq!(
        read_back("accepted x\u{1b}[31m on push\nrefused: forged"),
        "accepted x [31m on push"
    );
    assert_eq!(read_back(""), "");
    assert!(read_back(&"a".repeat(3 * VERDICT_MAX)).len() == VERDICT_MAX);
}
