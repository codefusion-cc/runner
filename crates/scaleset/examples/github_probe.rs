//! A check against the real GitHub through a console, for one throwaway repository: registers a scale set under the
//! machine's name, opens a session, registers an Actions runner and removes it by its name, waits for a queued job on
//! the long poll (offered, or assigned by the service itself), acquires it if it was only offered, fetches (and
//! removes) a one-job Actions runner configuration for it, then closes the session and deletes the scale set. Every
//! address the console hands back passes the client's check on the way, or the probe fails naming the host.
//!
//! `CF_RUNNER_TOKEN=cfr_… github_probe <console URL> <machine> <owner/name> <scale set suffix> <seconds> <label>...`
//!
//! The machine is one enrolled with the console, with GitHub through the console on; the token is its own. The console
//! holds the Runners App's key and the Actions service's admin token: the probe never sees either. It prints ids,
//! counts and statuses, never a token or an Actions runner configuration.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cf_console_client::{Client, ConsoleTokens};
use cf_ghapp::{API, GitHubApp, Http, RetryPolicy, SystemClock, SystemMonotonic};
use cf_scaleset::ScaleSetClient;
use cf_secret::Secret;

fn say(what: &str) {
    println!("{what}");
}

/// A name of an Actions runner of `machine`, as the console takes one: `<machine>-j` and 7 lowercase hex digits.
fn runner_name(machine: &str) -> String {
    format!("{machine}-j{:07x}", rand::random::<u32>() & 0x0fff_ffff)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let token = std::env::var("CF_RUNNER_TOKEN").ok().filter(|t| !t.is_empty());
    let (Some(token), true) = (token, args.len() >= 6) else {
        eprintln!(
            "usage: CF_RUNNER_TOKEN=cfr_… github_probe <console URL> <machine> <owner/name> <scale set suffix> <seconds> <label>..."
        );
        std::process::exit(2);
    };
    if let Err(e) = probe(&args, Secret::new(token)).await {
        eprintln!("FAILED: {e}");
        std::process::exit(1);
    }
}

async fn probe(args: &[String], token: Secret<String>) -> Result<(), Box<dyn std::error::Error>> {
    let (console, machine, repo, suffix) = (&args[0], &args[1], &args[2], &args[3]);
    let wait: u64 = args[4].parse()?;
    let labels: Vec<String> = args[5..].to_vec();
    let http = Http::new(Duration::from_secs(30), RetryPolicy::default());
    let console = Client::new(console, token, http.clone());
    let tokens = ConsoleTokens::new(
        console.clone(),
        machine,
        Arc::new(SystemClock),
        Arc::new(SystemMonotonic),
    );
    let app = GitHubApp::with_source(Arc::new(tokens), API, http);
    let installation = app.repo_installation(repo).await?;
    say(&format!("installation {installation} covers {repo}"));
    match app.variables(installation, repo).await {
        Ok(v) => say(&format!("variables: readable ({} found)", v.len())),
        Err(e) => say(&format!("variables: NOT readable ({e})")),
    }
    let client = ScaleSetClient::new(
        app,
        console,
        machine,
        installation,
        repo,
        &format!("https://github.com/{repo}"),
        Duration::from_secs(75),
    );
    let set = client
        .ensure_scale_set(&format!("{machine}--{suffix}"), &labels)
        .await?;
    say(&format!(
        "scale set {} registered with labels {:?}",
        set.id,
        set.label_names()
    ));
    let result = run(&client, machine, set.id, wait).await;
    // Whatever happened, the scale set goes.
    match client.delete_scale_set(set.id).await {
        Ok(deleted) => say(&format!("scale set {} deleted: {deleted}", set.id)),
        Err(e) => say(&format!("scale set {} NOT deleted: {e}", set.id)),
    }
    result
}

async fn run(
    client: &ScaleSetClient,
    machine: &str,
    id: i64,
    wait: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = client.open_session(id).await?;
    let stats = session.initial_statistics();
    say(&format!(
        "session open; at the start: available {}, assigned {}",
        stats.total_available_jobs, stats.total_assigned_jobs
    ));
    // An Actions runner is removed by its name, and is gone after: what a registration whose answer was lost is
    // cleaned up with.
    let lookup = runner_name(machine);
    let registered = client.generate_jit_config(id, &lookup).await?;
    say(&format!(
        "Actions runner {} registered; removed by its name: {}",
        registered.runner.id,
        client.remove_runner_named(&lookup).await?
    ));
    say(&format!(
        "after its removal the name finds nothing: {}",
        !client.remove_runner_named(&lookup).await?
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
            let jit = client.generate_jit_config(id, &runner_name(machine)).await?;
            say(&format!(
                "Actions runner {} registered, configuration of {} bytes fetched",
                jit.runner.id,
                jit.encoded.expose().len()
            ));
            say(&format!(
                "Actions runner removed: {}",
                client.remove_runner(jit.runner.id).await?
            ));
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
