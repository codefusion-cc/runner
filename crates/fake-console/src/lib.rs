//! A fake CodeFusion Console at the HTTP edge, for tests: the runner protocol's `enroll`, `report` and `leave`, over
//! real sockets, with the checks of the console's own code (`worker/runners/agent.ts` and `protocol.ts`) in the same
//! order, with the same statuses and bounds.
//!
//! Tests drive it ([`FakeConsole::add_code`], [`FakeConsole::pause`], [`FakeConsole::queue_command`]), make it
//! misbehave ([`FakeConsole::down`], [`FakeConsole::fail_next`], [`FakeConsole::accept_then_drop_next`]) and read
//! what it saw ([`FakeConsole::reports`], [`FakeConsole::job_events`]). It is written from the console's code and not
//! from the client's types, so the client's wire format is checked against the contract and not against itself.
//!
//! What it leaves out: Cloudflare Access in front of the path, the delivery windows of commands (a command comes
//! with every answer until a result names it), and the console's history, samples and audit log.

mod schema;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use cf_fake_github::httpd::{Handler, Reply, Request, Server};
use rand::Rng as _;
use serde_json::{Value, json};

/// The largest request body the console reads: 512 KiB.
pub const MAX_BODY: usize = 512 * 1024;

const PREFIX: &str = "/agent/runners/";
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";

struct Code {
    /// The Mac it was made for. The console binds every code to a name; a code made with [`FakeConsole::add_code`]
    /// has none and takes any.
    machine: Option<String>,
    used: bool,
}

struct Machine {
    id: u64,
    name: String,
    token: String,
    /// The system as the console keeps it: what the last connect request or report said, read by [`schema::os`].
    os: Option<&'static str>,
}

struct Pending {
    id: String,
    kind: String,
    args: Option<Value>,
    machine: Option<String>,
    done_by: HashSet<u64>,
}

#[derive(Default)]
struct Faults {
    down: bool,
    fail: Option<(u32, u16)>,
    drop_before: u32,
    drop_after: u32,
}

#[derive(Default)]
struct State {
    codes: Vec<(String, Code)>,
    machines: Vec<Machine>,
    next_machine: u64,
    tokens: Vec<String>,
    enrollments: Vec<Value>,
    reports: Vec<Value>,
    events: Vec<Value>,
    left: Vec<String>,
    commands: Vec<Pending>,
    calls: usize,
    paused: bool,
    watched: bool,
    faults: Faults,
}

/// A running fake console. It stops when dropped.
pub struct FakeConsole {
    state: Arc<Mutex<State>>,
    server: Server,
}

fn error(status: u16, message: &str) -> Reply {
    Reply::status_json(status, &json!({ "error": message }))
}

fn take(counter: &mut u32) -> bool {
    if *counter == 0 {
        return false;
    }
    *counter -= 1;
    true
}

/// A request's body as the console reads it: the content type, the size, then JSON (`agent.ts` `body` and
/// `http.ts` `readBody`).
fn json_body(request: &Request) -> Result<Value, Reply> {
    // agent.ts: `startsWith('application/json')`, case-sensitive.
    if !request
        .header("content-type")
        .is_some_and(|t| t.starts_with("application/json"))
    {
        return Err(error(415, "Expected JSON"));
    }
    // http.ts readBytes: a Content-Length over the limit, or a body that reads over it, is a 413.
    let declared = request
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if declared > MAX_BODY || request.body.len() > MAX_BODY {
        return Err(error(413, "Request too large"));
    }
    // http.ts readBody: TextDecoder drops a byte order mark and replaces bad UTF-8; JSON.parse failing is a 400.
    let text = String::from_utf8_lossy(&request.body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).map_err(|_| error(400, "Expected a JSON object"))
}

fn token_shaped(token: &str) -> bool {
    token.strip_prefix("cfr_").is_some_and(|rest| {
        rest.len() == 43
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

fn new_token() -> String {
    let mut rng = rand::rng();
    let tail: String = (0..43)
        .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
        .collect();
    format!("cfr_{tail}")
}

impl State {
    /// agent.ts `machineOf`: the Mac a bearer token belongs to, or a 401.
    fn machine_of(&self, request: &Request) -> Result<u64, Reply> {
        let token = request
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| token_shaped(t));
        token
            .and_then(|t| self.machines.iter().find(|m| m.token == t))
            .map(|m| m.id)
            .ok_or_else(|| error(401, "Unknown token"))
    }

    fn route(&mut self, request: &Request) -> Reply {
        // agent.ts handleAgent: the method first, then the path.
        if request.method != "POST" {
            return error(405, "Use POST");
        }
        match request.path.strip_prefix(PREFIX) {
            Some("enroll") => self.enroll(request),
            Some("report") => self.report(request),
            Some("leave") => self.leave(request),
            _ => error(404, "Not found"),
        }
    }

    /// agent.ts `enroll`.
    fn enroll(&mut self, request: &Request) -> Reply {
        let body = match json_body(request) {
            Ok(body) => body,
            Err(reply) => return reply,
        };
        if let Err(problem) = schema::hello(&body) {
            return error(
                400,
                &format!("Not a connect request this console reads: {problem}"),
            );
        }
        self.enrollments.push(body.clone());
        let code = body["code"].as_str().unwrap_or("").trim().to_string();
        let machine = body["machine"].as_str().unwrap_or("").to_string();
        let refused = "This code isn't valid: it was used, it expired, or it was cancelled. Make a new one on the console's Runners page.";
        let Some((_, entry)) = self.codes.iter_mut().find(|(c, e)| *c == code && !e.used) else {
            return error(401, refused);
        };
        // A code connects the Mac it was made for, and no other.
        if let Some(name) = entry.machine.as_ref().filter(|name| **name != machine) {
            return error(
                403,
                &format!(
                    "This code is for {name}, and this Mac is {machine}. Make a code for {machine}; this one still works for {name}."
                ),
            );
        }
        entry.used = true;
        let token = new_token();
        self.tokens.push(token.clone());
        match self.machines.iter_mut().find(|m| m.name == machine) {
            // A Mac connecting again under its name keeps its row; the old token stops working.
            Some(existing) => {
                existing.token = token.clone();
                existing.os = schema::os(&body);
            }
            None => {
                self.next_machine += 1;
                self.machines.push(Machine {
                    id: self.next_machine,
                    name: machine.clone(),
                    token: token.clone(),
                    os: schema::os(&body),
                });
            }
        }
        Reply::json(&json!({ "token": token, "name": machine }))
    }

    /// agent.ts `ingestReport`.
    fn report(&mut self, request: &Request) -> Reply {
        // `ingestReport(env, await machineOf(...), await body(...))`: the token is checked before the body.
        let machine = match self.machine_of(request) {
            Ok(machine) => machine,
            Err(reply) => return reply,
        };
        let body = match json_body(request) {
            Ok(body) => body,
            Err(reply) => return reply,
        };
        if let Err(problem) = schema::report(&body) {
            return error(400, &format!("Not a report this console reads: {problem}"));
        }
        self.reports.push(body.clone());
        if let Some(jobs) = body["jobs"].as_array() {
            self.events.extend(jobs.iter().cloned());
        }
        for result in body["results"].as_array().into_iter().flatten() {
            let id = result["id"].as_str().unwrap_or("");
            for command in self.commands.iter_mut().filter(|c| c.id == id) {
                if command
                    .machine
                    .as_deref()
                    .is_none_or(|name| self.machines.iter().any(|m| m.id == machine && m.name == name))
                {
                    command.done_by.insert(machine);
                }
            }
        }
        if let Some(row) = self.machines.iter_mut().find(|m| m.id == machine) {
            row.os = schema::os(&body);
        }
        // A Mac renamed on the Mac is renamed here too, unless another Mac has the name.
        let wanted = body["status"]["machine"].as_str().unwrap_or("").to_string();
        if !self.machines.iter().any(|m| m.name == wanted)
            && let Some(row) = self.machines.iter_mut().find(|m| m.id == machine)
        {
            row.name = wanted;
        }
        let name = self
            .machines
            .iter()
            .find(|m| m.id == machine)
            .map(|m| m.name.clone())
            .unwrap_or_default();
        let commands: Vec<Value> = self
            .commands
            .iter()
            .filter(|c| c.machine.as_deref().is_none_or(|m| m == name) && !c.done_by.contains(&machine))
            .map(|c| match &c.args {
                Some(args) => json!({ "id": c.id, "kind": c.kind, "args": args }),
                None => json!({ "id": c.id, "kind": c.kind }),
            })
            .collect();
        // REPORT_WATCHED_S while a command waits or the page is watched, REPORT_IDLE_S otherwise.
        let quick = !commands.is_empty() || self.watched;
        Reply::json(
            &json!({ "paused": self.paused, "reportEvery": if quick { 5 } else { 60 }, "commands": commands }),
        )
    }

    /// agent.ts `leave`: the Mac goes from the console; its token stops working with its row.
    fn leave(&mut self, request: &Request) -> Reply {
        let machine = match self.machine_of(request) {
            Ok(machine) => machine,
            Err(reply) => return reply,
        };
        if let Some(at) = self.machines.iter().position(|m| m.id == machine) {
            let gone = self.machines.remove(at);
            self.left.push(gone.name);
        }
        Reply::json(&json!({ "ok": true }))
    }
}

impl FakeConsole {
    /// Starts a console on a free port of the loopback interface, with no codes and no Macs.
    pub async fn start() -> FakeConsole {
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let handler: Handler = Arc::new(move |request| {
            let state = shared.clone();
            Box::pin(async move { FakeConsole::handle(&state, &request) })
        });
        FakeConsole {
            state,
            server: Server::start(handler).await,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn handle(state: &Mutex<State>, request: &Request) -> Reply {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.calls += 1;
        if state.faults.down {
            return error(503, "The console is down");
        }
        if take(&mut state.faults.drop_before) {
            return Reply::Drop;
        }
        if let Some((left, status)) = state.faults.fail {
            state.faults.fail = (left > 1).then_some((left - 1, status));
            return error(status, "The console fails on purpose");
        }
        let reply = state.route(request);
        let taken = matches!(&reply, Reply::Response { status: 200, .. });
        if taken && take(&mut state.faults.drop_after) {
            return Reply::Drop;
        }
        reply
    }

    /// `http://127.0.0.1:port`, the base address a client takes.
    pub fn url(&self) -> String {
        self.server.url()
    }

    /// Makes a one-time code that connects a Mac of any name. (The console makes every code for one name and answers
    /// 403 to another Mac; [`FakeConsole::add_code_for`] does that.)
    pub fn add_code(&self, code: &str) {
        self.lock().codes.push((
            code.trim().to_string(),
            Code {
                machine: None,
                used: false,
            },
        ));
    }

    /// Makes a one-time code for the Mac `machine` only, as the console does.
    pub fn add_code_for(&self, code: &str, machine: &str) {
        self.lock().codes.push((
            code.trim().to_string(),
            Code {
                machine: Some(machine.to_string()),
                used: false,
            },
        ));
    }

    /// While down, every call is answered 503 before anything else is looked at.
    pub fn down(&self, down: bool) {
        self.lock().faults.down = down;
    }

    /// The next `n` calls are answered `status` with an `error` text, before anything else is looked at.
    pub fn fail_next(&self, n: u32, status: u16) {
        self.lock().faults.fail = (n > 0).then_some((n, status));
    }

    /// The next `n` calls get a closed connection and nothing else, and the console does nothing with them.
    pub fn drop_before_next(&self, n: u32) {
        self.lock().faults.drop_before = n;
    }

    /// The next `n` calls that the console accepts are taken (kept, a code spent) and never answered: the connection
    /// closes without a byte.
    pub fn accept_then_drop_next(&self, n: u32) {
        self.lock().faults.drop_after = n;
    }

    /// Whether the console says every Mac is paused in its answers.
    pub fn pause(&self, paused: bool) {
        self.lock().paused = paused;
    }

    /// Whether someone watches the console's page: `reportEvery` is 5 instead of 60.
    pub fn watch(&self, watched: bool) {
        self.lock().watched = watched;
    }

    /// Queues a command for every Mac; it comes with each answer until a report carries a result with its id. Returns
    /// the id.
    pub fn queue_command(&self, kind: &str, args: Option<Value>) -> String {
        self.queue(None, kind, args)
    }

    /// Queues a command for the Mac named `machine` only. Returns the id.
    pub fn queue_command_for(&self, machine: &str, kind: &str, args: Option<Value>) -> String {
        self.queue(Some(machine.to_string()), kind, args)
    }

    /// Queues a command for every Mac under an id of the test's choosing, as a console that broke its own contract
    /// would send one: the real one makes the ids itself.
    pub fn queue_command_as(&self, id: &str, kind: &str, args: Option<Value>) {
        self.lock().commands.push(Pending {
            id: id.to_string(),
            kind: kind.to_string(),
            args,
            machine: None,
            done_by: HashSet::new(),
        });
    }

    fn queue(&self, machine: Option<String>, kind: &str, args: Option<Value>) -> String {
        let mut state = self.lock();
        let id = format!("cmd-{}", state.commands.len() + 1);
        state.commands.push(Pending {
            id: id.clone(),
            kind: kind.to_string(),
            args,
            machine,
            done_by: HashSet::new(),
        });
        id
    }

    /// Every report the console accepted, as the JSON it received. A refused one is not here: the console keeps
    /// nothing of it.
    pub fn reports(&self) -> Vec<Value> {
        self.lock().reports.clone()
    }

    /// Every job event of every accepted report, in the order received, duplicates included, as received (so with
    /// the fields the console drops today).
    pub fn job_events(&self) -> Vec<Value> {
        self.lock().events.clone()
    }

    /// The `id` of each of [`FakeConsole::job_events`], in the same order (empty for an event without one).
    pub fn job_event_ids(&self) -> Vec<String> {
        self.lock()
            .events
            .iter()
            .map(|e| e["id"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// Every connect request the console read as valid, as received, whether its code was good or not.
    pub fn enrollments(&self) -> Vec<Value> {
        self.lock().enrollments.clone()
    }

    /// Every token the console issued, in order. A Mac that enrols again gets a new one and loses the old one.
    pub fn tokens(&self) -> Vec<String> {
        self.lock().tokens.clone()
    }

    /// The names of the Macs that left.
    pub fn left(&self) -> Vec<String> {
        self.lock().left.clone()
    }

    /// The names of the Macs that are connected now.
    pub fn machines(&self) -> Vec<String> {
        self.lock().machines.iter().map(|m| m.name.clone()).collect()
    }

    /// The system the console keeps for this machine: `macos`, `linux`, or none when it was not said or is one the
    /// console does not know.
    pub fn os_of(&self, machine: &str) -> Option<String> {
        self.lock()
            .machines
            .iter()
            .find(|m| m.name == machine)
            .and_then(|m| m.os)
            .map(String::from)
    }

    /// How many requests arrived, refused ones and faults included.
    pub fn calls(&self) -> usize {
        self.lock().calls
    }
}
