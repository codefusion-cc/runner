//! `github/read`, `github/registration` and `github/jit` (the console's `worker/runners/github.ts`): the console holds
//! the Runners App's key and hands a machine its GitHub tokens, made by the attached [`FakeGithub`] as the console makes
//! them with that key, so a token from here works against that fake as one from the console works against GitHub.
//!
//! Kept as the console keeps them: a read or registration token is handed out again while more than 40 minutes of it
//! are left, but a read token the machine names `refused` is replaced by a new one and revoked at GitHub; one-job
//! runners are limited to 3 a repository and 20 a machine in five minutes. The switch's 403 carries the code
//! `broker_off`; a runner name not of the machine's is a 403 without one. Time is the fake GitHub's clock, so a test
//! that moves it moves this too.

use cf_fake_github::FakeGithub;
use cf_fake_github::httpd::{Reply, Request};
use serde_json::{Value, json};

use crate::{State, coded, error, json_body, schema};

/// A kept token is handed out again only while more than this is left of it (`REUSE_MIN_MS`), in seconds.
pub const REUSE_MIN_S: i64 = 40 * 60;
/// The window of the one-job runner limits (`JIT_WINDOW_MS`), in seconds.
pub const JIT_WINDOW_S: i64 = 5 * 60;
/// One-job runners a repository may get in the window.
pub const JIT_PER_REPO: usize = 3;
/// One-job runners a machine may get in the window, over all its repositories.
pub const JIT_PER_MACHINE: usize = 20;

/// One call to a `github/…` route that got past the machine's token and the body's content type, size and JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct GithubCall {
    /// `read`, `registration` or `jit`.
    pub route: String,
    /// The machine that called, by name.
    pub machine: String,
    /// The JSON it sent.
    pub body: Value,
    /// The status the console answered.
    pub status: u16,
}

/// A token the console keeps for a machine.
pub(crate) struct Kept {
    machine: u64,
    kind: &'static str,
    scope: String,
    token: String,
    expires_at: i64,
}

/// One one-job runner asked for, counted against the limits whatever GitHub answered.
pub(crate) struct Attempt {
    machine: u64,
    repo: String,
    at: i64,
}

impl State {
    /// agent.ts `callerOf`, then the route's own checks in github.ts's order: the machine's token (401), the body
    /// (415, 413, 400), the machine's switch (403 `broker_off`), the schema (400), for a runner its name (403), the
    /// Runners App (409).
    pub(crate) fn github(&mut self, route: &str, request: &Request) -> Reply {
        let machine = match self.machine_of(request) {
            Ok(machine) => machine,
            Err(reply) => return reply,
        };
        let body = match json_body(request) {
            Ok(body) => body,
            Err(reply) => return reply,
        };
        let name = self.name_of(machine);
        let reply = match self.override_next.take() {
            Some(reply) => reply,
            None => self.github_route(route, machine, &name, &body),
        };
        let status = match &reply {
            Reply::Response { status, .. } => *status,
            _ => 0,
        };
        self.github_calls.push(GithubCall {
            route: route.to_string(),
            machine: name,
            body,
            status,
        });
        reply
    }

    fn name_of(&self, machine: u64) -> String {
        self.machines
            .iter()
            .find(|m| m.id == machine)
            .map(|m| m.name.clone())
            .unwrap_or_default()
    }

    fn github_route(&mut self, route: &str, machine: u64, name: &str, body: &Value) -> Reply {
        let on = self.machines.iter().any(|m| m.id == machine && m.broker);
        if !on {
            return coded(
                403,
                &format!(
                    "GitHub through the console is off for {name}: the owner turns it on on the machine's page in the console"
                ),
                "broker_off",
            );
        }
        let checked = match route {
            "read" => schema::github_read(body),
            "registration" => schema::github_registration(body),
            _ => schema::github_jit(body),
        };
        if let Err(problem) = checked {
            return error(400, &format!("Not a request this console reads: {problem}"));
        }
        let runner = body["name"].as_str().unwrap_or("");
        if route == "jit" && schema::runner_machine(runner) != Some(name) {
            return error(
                403,
                &format!("A runner of {name} is named {name}-j and seven hex digits, not {runner}"),
            );
        }
        let Some(github) = self.github.clone() else {
            return error(
                409,
                "The console holds no Runners App: an owner makes it on the GitHub Apps page",
            );
        };
        match route {
            "read" => {
                let refused: Vec<String> = body["refused"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect();
                self.read(&github, machine, &refused)
            }
            "registration" => self.registration(&github, machine, body["repo"].as_str().unwrap_or("")),
            _ => self.jit(&github, machine, body),
        }
    }

    /// The token kept for `machine` of `kind` and `scope` while more than [`REUSE_MIN_S`] of it is left and it is not
    /// one of `refused`, else one `make` makes, kept in its place. A kept read token named refused is revoked at
    /// `github` once its replacement is kept.
    fn kept(
        &mut self,
        github: &FakeGithub,
        machine: u64,
        kind: &'static str,
        scope: &str,
        refused: &[String],
        make: impl FnOnce() -> Option<(String, i64)>,
    ) -> Option<(String, i64)> {
        let now = github.now_epoch();
        self.kept.retain(|k| k.expires_at > now);
        let found = self.kept.iter().find(|k| {
            k.machine == machine && k.kind == kind && k.scope == scope && k.expires_at - REUSE_MIN_S > now
        });
        let mut replaced = None;
        if let Some(k) = found {
            if !refused.contains(&k.token) {
                return Some((k.token.clone(), k.expires_at));
            }
            replaced = Some(k.token.clone());
        }
        let (token, expires_at) = make()?;
        if let Some(old) = replaced.filter(|_| kind == "read") {
            github.revoke_token(&old);
        }
        self.kept
            .retain(|k| !(k.machine == machine && k.kind == kind && k.scope == scope));
        self.kept.push(Kept {
            machine,
            kind,
            scope: scope.to_string(),
            token: token.clone(),
            expires_at,
        });
        Some((token, expires_at))
    }

    fn read(&mut self, github: &FakeGithub, machine: u64, refused: &[String]) -> Reply {
        let now = github.now_epoch();
        let refused = if self.ignore_refused { &[][..] } else { refused };
        let mut installations = vec![];
        let listed = self
            .installations
            .clone()
            .unwrap_or_else(|| github.installations());
        for (id, account) in listed {
            let Some((token, expires_at)) =
                self.kept(github, machine, "read", &id.to_string(), refused, || {
                    Some(github.mint_read_token())
                })
            else {
                continue;
            };
            installations.push(json!({
                "id": id, "account": account, "token": token, "expiresInS": (expires_at - now).max(0),
            }));
        }
        Reply::json(&json!({ "installations": installations }))
    }

    fn registration(&mut self, github: &FakeGithub, machine: u64, repo: &str) -> Reply {
        let now = github.now_epoch();
        let scope = repo.to_lowercase();
        match self.kept(github, machine, "registration", &scope, &[], || {
            github.mint_registration_token(repo)
        }) {
            Some((token, expires_at)) => {
                Reply::json(&json!({ "token": token, "expiresInS": (expires_at - now).max(0) }))
            }
            None => error(404, "Not Found"),
        }
    }

    fn jit(&mut self, github: &FakeGithub, machine: u64, body: &Value) -> Reply {
        let repo = body["repo"].as_str().unwrap_or("");
        let runner = body["name"].as_str().unwrap_or("");
        let now = github.now_epoch();
        let scope = repo.to_lowercase();
        self.attempts.retain(|a| a.at > now - JIT_WINDOW_S);
        let mine = || self.attempts.iter().filter(|a| a.machine == machine);
        let first_of_repo = mine().filter(|a| a.repo == scope).map(|a| a.at).min();
        let first = mine().map(|a| a.at).min();
        let over = if mine().filter(|a| a.repo == scope).count() >= JIT_PER_REPO {
            first_of_repo
        } else if mine().count() >= JIT_PER_MACHINE {
            first
        } else {
            None
        };
        if let Some(first) = over {
            let wait = (first + JIT_WINDOW_S - now).max(1);
            return error(
                429,
                &format!("Too many runners asked for in five minutes: ask again in {wait} s"),
            )
            .with_header("retry-after", &wait.to_string());
        }
        self.attempts.push(Attempt {
            machine,
            repo: scope,
            at: now,
        });
        let labels: Vec<String> = body["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(String::from))
            .collect();
        match github.mint_plain_jit(repo, runner, &labels) {
            Ok((id, config)) => Reply::json(&json!({ "runnerId": id, "encodedJitConfig": config })),
            Err(404) => error(404, "Not Found"),
            Err(_) => error(409, "Already exists - A runner with this name already exists."),
        }
    }

    /// Turning GitHub through the console off forgets the machine's kept tokens and revokes its read tokens.
    pub(crate) fn broker_off(&mut self, machine: u64) {
        let (gone, kept): (Vec<Kept>, Vec<Kept>) = std::mem::take(&mut self.kept)
            .into_iter()
            .partition(|k| k.machine == machine);
        self.kept = kept;
        if let Some(github) = &self.github {
            for k in gone.iter().filter(|k| k.kind == "read") {
                github.revoke_token(&k.token);
            }
        }
    }
}
