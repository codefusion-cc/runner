//! A check against the real GitHub, for one throwaway repository: registers a scale set under its own name, opens a
//! session, waits for a queued job on the long poll, acquires it, fetches (and removes) a one-job runner
//! configuration for it, then closes the session and deletes the scale set.
//!
//! `github_probe <credentials folder> <owner/name> <scale set name> <seconds to wait> <label>...`
//!
//! The folder holds `app.json` and `app.pem` (see `cf_ghapp::AppCredentials::load`). It prints ids, counts and
//! statuses, never a token, a key or a runner configuration.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cf_ghapp::{API, AppCredentials, GitHubApp, Http, RetryPolicy, SystemClock, http_client};
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
        Http::new(http_client(Duration::from_secs(30)), RetryPolicy::default()),
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
        Arc::new(SystemClock),
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
        if let Some(job) = message.available.first() {
            offered = true;
            say(&format!(
                "offered: request {} of {} ({}), labels {:?}",
                job.runner_request_id,
                job.repository(),
                job.job_display_name,
                job.request_labels
            ));
            let got = session.acquire_jobs(&[job.runner_request_id]).await?;
            say(&format!("acquired {got:?}"));
            let jit = client
                .generate_jit_config(id, "cf-runner-probe-j0000001", "_work")
                .await?;
            say(&format!(
                "runner {} registered, configuration of {} bytes fetched",
                jit.runner.id,
                jit.encoded.expose().len()
            ));
            client.remove_runner(jit.runner.id).await?;
            say("runner removed");
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
