//! `fakes serve`: GitHub and the console as this repository's fakes, for a fresh host set up by
//! `scripts/host-setup.sh` the way a cloud VM is: a connect code and no App key, its GitHub tokens from the console.
//! It writes the code and the two addresses, then waits until a host enrolled with that code serves the fake's
//! repository (a scale set with a session open at GitHub, and the console told so), and ends with what it saw.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cf_fake_console::FakeConsole;
use cf_fake_github::{FakeGithub, Options};
use cf_harness::REPO;
use serde_json::json;

use crate::Result;

const USAGE: &str = "usage: fakes serve --code-file <file> --urls-file <file> [--timeout-s N]";
const WORKFLOW: &str = "on: push\njobs:\n  build:\n    runs-on: [self-hosted, cf-2g]\n    steps: []\n";

pub fn run(args: &[String]) -> Result<()> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let (mut code_file, mut urls_file, mut timeout) = (None, None, Duration::from_secs(1800));
    let mut rest = match words.as_slice() {
        ["serve", rest @ ..] => rest,
        _ => return Err(USAGE.into()),
    };
    while let [flag, value, more @ ..] = rest {
        match *flag {
            "--code-file" => code_file = Some(PathBuf::from(value)),
            "--urls-file" => urls_file = Some(PathBuf::from(value)),
            "--timeout-s" => timeout = Duration::from_secs(value.parse()?),
            _ => return Err(USAGE.into()),
        }
        rest = more;
    }
    let (Some(code_file), Some(urls_file), []) = (code_file, urls_file, rest) else {
        return Err(USAGE.into());
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (github, console) = runtime.block_on(async {
        let github = FakeGithub::start(Options {
            poll_timeout: Duration::from_secs(5),
            ..Options::default()
        })
        .await;
        (github, FakeConsole::start().await)
    });
    // As long as GitHub's own tokens run now, past the 255 characters a Runner once refused.
    github.set_read_token_length(1000);
    github.add_repo(REPO, true);
    github.set_workflow(REPO, "ci.yml", WORKFLOW);
    console.serve_github(github.clone());
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let code = format!("fresh-host-{nanos:x}");
    console.add_code(&code);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&code_file)?
        .write_all(format!("{code}\n").as_bytes())?;
    fs::write(
        &urls_file,
        format!("CONSOLE={}\nGITHUB={}\n", console.url(), github.url()),
    )?;
    println!(
        "fakes: the console at {}, GitHub at {}",
        console.url(),
        github.url()
    );

    let began = Instant::now();
    let mut said = (false, false);
    while began.elapsed() < timeout {
        let machines = console.machines();
        if let Some(machine) = machines.first() {
            if !said.0 {
                // The owner's tap that lets the console hand the machine GitHub tokens.
                console.set_broker(machine, true);
                println!("fakes: {machine} enrolled after {}s", began.elapsed().as_secs());
                said.0 = true;
            }
            let session = github.scale_sets().iter().any(|set| set.has_session);
            let told = console.reports().iter().any(|report| {
                report["status"]["machine"] == json!(machine)
                    && report["status"]["scaleSets"][0]["repo"] == json!(REPO)
            });
            if session && told {
                println!(
                    "fakes: {machine} serves {REPO} after {}s, with GitHub tokens from the console alone",
                    began.elapsed().as_secs()
                );
                return Ok(());
            }
            if session && !said.1 {
                println!("fakes: a session is open at GitHub; waiting for the console's report");
                said.1 = true;
            }
        }
        sleep(Duration::from_secs(1));
    }
    let calls: Vec<String> = console
        .github_calls()
        .iter()
        .map(|call| format!("{call:?}"))
        .collect();
    Err(format!(
        "no host served {REPO} in {}s; enrolled: {:?}; scale sets: {:?}; the console's GitHub calls: {calls:?}",
        timeout.as_secs(),
        console.machines(),
        github.scale_sets()
    )
    .into())
}
