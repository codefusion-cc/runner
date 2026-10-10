//! A check against the real GitHub, for one throwaway repository: registers a scale set under its own name, opens a
//! session, registers an Actions runner, finds it by its name and removes it, waits for a queued job on the long
//! poll (offered, or assigned by the service itself), acquires it if it was only offered, fetches (and removes) a
//! one-job Actions runner configuration for it, then closes the session and deletes the scale set. Every address the
//! service hands back passes the client's check on the way, or the probe fails naming the host.
//!
//! `github_probe <credentials folder> <owner/name> <scale set name> <seconds to wait> <label>...`
//!
//! The folder holds `app.json` and `app.pem` (see `cf_ghapp::AppCredentials::load`). It prints ids, counts and
//! statuses, never a token, a key or an Actions runner configuration.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cf_ghapp::{API, AppCredentials, GitHubApp, Http, RetryPolicy, SystemClock};
use cf_scaleset::ScaleSetClient;

fn say(what: &str) {
    println!("{what}");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 5 {
        eprintln!(
            "usage: github_probe <credentials folder> <owner/name> <scale set name> <seconds to wait> <label>..."
        );
        std::process::exit(2);
    }
    if let Err(e) = probe(&args).await {
        eprintln!("FAILED: {e}");
        std::process::exit(1);
    }
}

async fn probe(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (dir, repo, name) = (&args[0], &args[1], &args[2]);
    let wait: u64 = args[3].parse()?;
    let labels: Vec<String> = args[4..].to_vec();
    let credentials = AppCredentials::load(Path::new(dir))?;
    let app = GitHubApp::from_credentials(
        credentials,
        API,
        Http::new(Duration::from_secs(30), RetryPolicy::default()),
        Arc::new(SystemClock),
    );
    let installation = app.repo_installation(repo).await?;
    say(&format!("installation {installation} covers {repo}"));
    let all = app.repositories(installation).await?;
    say(&format!(
        "the installation reaches {} repositories; this one is {}",
        all.len(),
        all.iter().any(|r| r.full_name == *repo)
    ));
    match app.variables(installation, repo).await {
        Ok(v) => say(&format!("variables: readable ({} found)", v.len())),
        Err(e) => say(&format!("variables: NOT readable ({e})")),
    }
    let client = ScaleSetClient::new(
        app,
        installation,
        repo,
        &format!("https://github.com/{repo}"),
        Duration::from_secs(75),
    );
    let set = client.ensure_scale_set(name, &labels).await?;
    say(&format!(
        "scale set {} registered with labels {:?}",
        set.id,
        set.label_names()
    ));
    let result = run(&client, set.id, wait).await;
    // Whatever happened, the scale set goes.
    match client.delete_scale_set(set.id).await {
        Ok(()) => say(&format!("scale set {} deleted", set.id)),
        Err(e) => say(&format!("scale set {} NOT deleted: {e}", set.id)),
    }
    result
}

async fn run(client: &ScaleSetClient, id: i64, wait: u64) -> Result<(), Box<dyn std::error::Error>> {
    let session = client.open_session(id, "cf-runner-probe").await?;
    let stats = session.initial_statistics();
    say(&format!(
        "session open; at the start: available {}, assigned {}",
        stats.total_available_jobs, stats.total_assigned_jobs
    ));
    // An Actions runner is found by its name and gone after its removal: what a registration whose answer was lost
    // is cleaned up with.
    let lookup = "cf-runner-probe-l0000001";
    let registered = client.generate_jit_config(id, lookup, "_work").await?;
    let found = client.runner_by_name(lookup).await?;
    say(&format!(
        "Actions runner {} found by its name: {}",
        registered.runner.id,
        found.as_ref() == Some(&registered.runner)
    ));
    client.remove_runner(registered.runner.id).await?;
    say(&format!(
        "after its removal the name finds nothing: {}",
        client.runner_by_name(lookup).await?.is_none()
    ));
    let deadline = Instant::now() + Duration::from_secs(wait);
    let mut after = 0;
    let mut outcome: Result<(), Box<dyn std::error::Error>> = Err("no job was offered in time".into());
    let mut offered = stats.total_available_jobs > 0;
    while Instant::now() < deadline {
        let Some(message) = session.poll(after, 1).await? else {
            say("the long poll ended quietly (202)");
            continue;
        };
        say(&format!(
            "message {}: {} offered, {} assigned, {} started, {} completed, {} skipped",
            message.id,
            message.available.len(),
            message.assigned.len(),
            message.started.len(),
            message.completed.len(),
            message.skipped
        ));
        if std::env::var_os("PROBE_RAW").is_some() {
            say(&format!("raw batch: {}", message.raw));
        }
        for done in &message.completed {
            say(&format!(
                "completed: request {} result {:?} by Actions runner {:?}",
                done.runner_request_id, done.result, done.runner_name
            ));
        }
        // With room for the job (the poll says how many Actions runners the machine would take) the service assigns it
        // by itself; without, it only offers it and it has to be acquired first.
        let mut job = None;
        if let Some(offer) = message.available.first() {
            offered = true;
            say(&format!(
                "offered: request {} of {} ({}), labels {:?}",
                offer.runner_request_id,
                offer.repository(),
                offer.job_display_name,
                offer.request_labels
            ));
            let got = session.acquire_jobs(&[offer.runner_request_id]).await?;
            say(&format!("acquired {got:?}"));
            job = Some(offer.clone());
        } else if let Some(assigned) = message.assigned.first() {
            offered = true;
            say(&format!(
                "assigned: request {} of {} ({}), labels {:?}",
                assigned.runner_request_id,
                assigned.repository(),
                assigned.job_display_name,
                assigned.request_labels
            ));
            job = Some(assigned.clone());
        }
        if job.is_some() {
            let jit = client
                .generate_jit_config(id, "cf-runner-probe-j0000001", "_work")
                .await?;
            say(&format!(
                "Actions runner {} registered, configuration of {} bytes fetched",
                jit.runner.id,
                jit.encoded.expose().len()
            ));
            client.remove_runner(jit.runner.id).await?;
            say("Actions runner removed");
            outcome = Ok(());
        }
        session.acknowledge(message.id).await?;
        after = message.id;
        if outcome.is_ok() {
            break;
        }
    }
    if offered && outcome.is_err() {
        say("a job was counted as available at the start but no message offered it");
    }
    session.close().await?;
    say("session closed");
    outcome
}
