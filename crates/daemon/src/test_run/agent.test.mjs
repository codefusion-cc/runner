// The test run's agent against a fake console on this machine, with real git bundles and real bash suites: what it
// sends, in what order and from which offset, how it ends, and that what it stops is gone. `node --test` in this
// folder. Only the console is a stand-in; the waits the agent keeps are shortened by its CF_RUN_*_MS knobs.

import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { after, test as nodeTest } from 'node:test';
import { setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

const AGENT = path.join(path.dirname(fileURLToPath(import.meta.url)), 'agent.mjs');
const RUN = 'tr_abcdefghijklmnopqrstuvwxyz';
const TOKEN = 'cfrt_' + 'A'.repeat(43);
const BASE = `/agent/runners/runs/${RUN}`;

/** Every agent a test started: one a test left running when it failed is killed at the end. */
const agents = new Set();
after(() => agents.forEach((child) => child.kill('SIGKILL')));
/** A test that hangs (an agent that never ends) fails after half a minute. */
const test = (name, options, fn) =>
  fn ? nodeTest(name, { timeout: 30_000, ...options }, fn) : nodeTest(name, { timeout: 30_000 }, options);

/** A repository of two commits, as two bundles: the first commit, and the second on top of it. */
function repository() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cf-repo-'));
  const git = (...args) =>
    execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@example.com', ...args], { cwd: dir }).toString().trim();
  git('init', '-q', '-b', 'main');
  fs.writeFileSync(path.join(dir, 'file.txt'), 'one\n');
  git('add', '.');
  git('commit', '-q', '-m', 'one');
  const first = git('rev-parse', 'HEAD');
  git('bundle', 'create', '-q', 'b0', 'main');
  fs.writeFileSync(path.join(dir, 'file.txt'), 'two\n');
  git('commit', '-q', '-am', 'two');
  git('bundle', 'create', '-q', 'b1', `${first}..main`);
  const bundles = ['b0', 'b1'].map((name) => fs.readFileSync(path.join(dir, name)));
  const commit = git('rev-parse', 'HEAD');
  fs.rmSync(dir, { recursive: true, force: true });
  return { bundles, commit };
}

const REPO = repository();
const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

/**
 * The console's side of the contract: the spec, the bundles' parts, the output by offset, the end. `fault(kind, state, body)`
 * may answer instead: a status, `'store-then-503'` (the output is kept, the answer lost), or nothing.
 */
async function fakeConsole({ suite, image = null, timeoutS = 60, partBytes = 300, badSha = false, fault, delayParts = 0 }) {
  const state = {
    stored: Buffer.alloc(0),
    posts: [],
    ends: [],
    cancel: false,
    conflicts: 0,
    invalid: 0,
    spec: {
      commit: REPO.commit,
      suite,
      image,
      timeoutS,
      partBytes,
      bundles: REPO.bundles.map((bytes) => ({
        sha256: badSha ? '0'.repeat(64) : sha(bytes),
        size: bytes.length,
        parts: Math.max(1, Math.ceil(bytes.length / partBytes)),
      })),
    },
  };
  const server = http.createServer(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    const reply = (status, value) => {
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(JSON.stringify(value));
    };
    // Where a sign-in page in front of the console sends what it stops: a page, not the console's answer.
    if (req.url === '/login') {
      res.writeHead(200, { 'content-type': 'text/html' });
      return res.end('<html>sign in</html>');
    }
    if (req.headers.authorization !== `Bearer ${TOKEN}`) return reply(401, {});
    const url = new URL(req.url, 'http://x');
    if (!url.pathname.startsWith(BASE)) return reply(404, {});
    const route = url.pathname.slice(BASE.length);
    const kind = route === '/spec' ? 'spec' : route === '/output' ? 'output' : route === '/end' ? 'end' : 'part';
    const faulted = fault?.(kind, state, body);
    if (typeof faulted === 'number') return reply(faulted, {});
    if (faulted && typeof faulted === 'object') {
      res.writeHead(faulted.status, faulted.headers ?? {});
      return res.end(faulted.body ?? '');
    }
    if (req.method === 'GET' && kind === 'spec') return reply(200, state.spec);
    const part = route.match(/^\/bundles\/(\d+)\/parts\/(\d+)$/);
    if (req.method === 'GET' && part) {
      if (delayParts) await sleep(delayParts);
      const [i, j] = [Number(part[1]), Number(part[2])];
      res.writeHead(200, { 'content-type': 'application/octet-stream' });
      return res.end(REPO.bundles[i].subarray(j * partBytes, (j + 1) * partBytes));
    }
    if (req.method === 'POST' && kind === 'output') {
      const offset = Number(url.searchParams.get('offset'));
      let invalid = false;
      try {
        new TextDecoder('utf-8', { fatal: true }).decode(body);
      } catch {
        invalid = true;
        state.invalid++;
      }
      state.posts.push({ offset, length: body.length, stored: state.stored.length });
      if (invalid && offset === state.stored.length) return reply(422, {});
      if (offset > state.stored.length) {
        state.conflicts++;
        return reply(409, { have: state.stored.length });
      }
      const past = body.subarray(state.stored.length - offset);
      state.stored = Buffer.concat([state.stored, past]);
      if (faulted === 'store-then-503') return reply(503, {});
      return reply(200, { have: state.stored.length, cancel: state.cancel });
    }
    if (req.method === 'POST' && kind === 'end') {
      state.ends.push({ ...JSON.parse(body.toString()), stored: state.stored.length });
      return reply(200, { ok: true });
    }
    reply(404, {});
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  state.url = `http://127.0.0.1:${server.address().port}${BASE}`;
  state.close = () => new Promise((resolve) => server.close(resolve));
  return state;
}

/** Starts the agent against `console` in a folder of its own. Says the folder, and its exit code once it exits. */
function start(console, { timeoutS = 60, knobs = {}, prepare } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cf-agent-'));
  prepare?.(dir);
  const env = { ...process.env };
  for (const name of Object.keys(env)) if (name.startsWith('CF_')) delete env[name];
  Object.assign(env, {
    CF_RUN_URL: console.url,
    CF_RUN_TOKEN: TOKEN,
    CF_RUN_TIMEOUT_S: String(timeoutS),
    CF_RUN_DIR: dir,
    CF_RUN_HEARTBEAT_MS: '100',
    CF_RUN_POLL_MS: '20',
    CF_RUN_BACKOFF_MIN_MS: '20',
    CF_RUN_BACKOFF_MAX_MS: '100',
    CF_RUN_KILL_GRACE_MS: '300',
    ...knobs,
  });
  const child = spawn(process.execPath, [AGENT], { env, stdio: ['ignore', 'ignore', 'pipe'] });
  agents.add(child);
  let stderr = '';
  child.stderr.on('data', (data) => (stderr += data));
  const done = new Promise((resolve) =>
    child.on('exit', (code) => {
      agents.delete(child);
      resolve({ code, dir, stderr });
    }),
  );
  return { dir, done };
}

/** Runs the agent against `console` until it exits. */
const agent = (console, options) => start(console, options).done;

/**
 * Whether a process of this number is gone, within two seconds. A zombie counts as gone: it runs nothing and holds
 * nothing, and in a container whose first process reaps no orphans (`docker run` without `--init`) it stays one.
 */
async function gone(pid) {
  for (let i = 0; i < 100; i++) {
    try {
      process.kill(pid, 0);
      if (/^\d+ \(.*\) Z /s.test(fs.readFileSync(`/proc/${pid}/stat`, 'utf8'))) return true;
    } catch (error) {
      if (error.code === 'ESRCH') return true;
    }
    await sleep(20);
  }
  return false;
}

/** The number a suite wrote to `../pid` once it is there. */
async function pidIn(dir) {
  const file = path.join(dir, 'pid');
  for (let i = 0; i < 500 && !fs.existsSync(file); i++) await sleep(10);
  return Number(fs.readFileSync(file, 'utf8'));
}

/** Every post went on from what the console had: no gap, no byte sent twice. */
function contiguous(posts) {
  return posts.every((post) => post.offset === post.stored);
}

test('a suite that passes: the checked-out commit, its output in order from offset 0, exit 0', async () => {
  const console = await fakeConsole({ suite: 'cat file.txt; echo out; echo err >&2; echo end; git rev-parse HEAD' });
  const { code, stderr } = await agent(console);
  await console.close();
  assert.equal(code, 0, stderr);
  const output = `two\nout\nerr\nend\n${REPO.commit}\n`;
  assert.equal(console.stored.toString(), output);
  assert.deepEqual(console.ends, [{ exit: 0, reason: 'exited', tail: output, stored: Buffer.byteLength(output) }]);
  assert.ok(contiguous(console.posts), JSON.stringify(console.posts));
  assert.equal(console.conflicts, 0);
});

test('a suite that fails ends with its own exit, and a signal with 128 and its number', async () => {
  let console = await fakeConsole({ suite: 'echo failing; exit 3' });
  await agent(console);
  await console.close();
  assert.equal(console.ends[0].exit, 3);
  assert.equal(console.ends[0].reason, 'exited');
  assert.equal(console.ends[0].tail, 'failing\n');

  console = await fakeConsole({ suite: 'kill -TERM $$' });
  await agent(console);
  await console.close();
  assert.equal(console.ends[0].exit, 143);
  assert.equal(console.ends[0].reason, 'exited');
});

test('the suite sees CI and CF_TEST_REMOTE, and none of the run\'s own variables', async () => {
  const console = await fakeConsole({ suite: "env | grep -E '^(CF_|CI=)' | sort" });
  await agent(console);
  await console.close();
  assert.equal(console.stored.toString(), 'CF_TEST_REMOTE=1\nCI=true\n');
});

test('an empty output is an empty end, and heartbeats go while the suite says nothing', async () => {
  const console = await fakeConsole({ suite: 'sleep 0.6' });
  await agent(console);
  await console.close();
  assert.equal(console.stored.length, 0);
  assert.deepEqual(console.ends, [{ exit: 0, reason: 'exited', tail: '', stored: 0 }]);
  assert.ok(console.posts.length >= 3, `${console.posts.length} heartbeats`);
  assert.ok(console.posts.every((post) => post.length === 0 && post.offset === 0));
});

test('a large output goes in posts of 256 KiB at most, byte for byte, and the tail is its last 64 KiB', async () => {
  // 300 000 bytes of a three-byte character: 256 KiB is no multiple of three, so the first post must be cut short.
  const console = await fakeConsole({ suite: "yes '€' | head -n 100000 | tr -d '\\n'; echo; echo done >&2" });
  await agent(console);
  await console.close();
  const expected = '€'.repeat(100000) + '\ndone\n';
  assert.equal(console.stored.toString(), expected);
  assert.ok(console.posts.every((post) => post.length <= 256 * 1024));
  assert.equal(console.invalid, 0, 'a post split a character');
  assert.ok(contiguous(console.posts));
  const tail = console.ends[0].tail;
  assert.ok(Buffer.byteLength(tail) <= 64 * 1024 && Buffer.byteLength(tail) > 64 * 1024 - 4);
  assert.ok(expected.endsWith(tail));
});

test('a character the suite writes in two goes whole', async () => {
  const console = await fakeConsole({ suite: "printf 'a\\xe2\\x82'; sleep 0.5; printf '\\xac b \\xf0\\x9f'; sleep 0.5; printf '\\x98\\x80\\n'" });
  await agent(console);
  await console.close();
  assert.equal(console.stored.toString(), 'a€ b 😀\n');
  assert.equal(console.invalid, 0, 'a post split a character');
  assert.ok(console.posts.some((post) => post.length === 1), 'the first byte went before the rest came');
});

test('a console that answers 503 for a while, and one that keeps output and loses its answer: nothing lost or doubled', async () => {
  let outputs = 0;
  let began = 0;
  let lost = false;
  const console = await fakeConsole({
    suite: 'for i in $(seq 1 40); do echo "line $i"; sleep 0.03; done',
    fault: (kind, state, body) => {
      if (kind === 'spec' && began++ < 3) return 503;
      if (kind !== 'output') return undefined;
      outputs++;
      // Output, not a heartbeat: its answer is lost after the console kept it.
      if (!lost && body.length > 0 && state.stored.length > 0) {
        lost = true;
        return 'store-then-503';
      }
      if (outputs >= 6 && outputs < 16) return outputs % 2 ? 503 : 502;
      if (outputs === 20) return 429;
      return undefined;
    },
  });
  await agent(console);
  await console.close();
  const expected = Array.from({ length: 40 }, (_, i) => `line ${i + 1}\n`).join('');
  assert.equal(console.stored.toString(), expected);
  assert.equal(console.ends[0].exit, 0);
  assert.equal(console.ends[0].stored, expected.length, 'the end came after all output was there');
  assert.ok(console.posts.some((post) => post.offset < post.stored), 'the lost answer was sent again from before');
});

test('a console that lost output answers 409 and gets it again from what it has', async () => {
  let dropped = false;
  const console = await fakeConsole({
    suite: 'for i in $(seq 1 30); do echo "line $i"; sleep 0.02; done',
    fault: (kind, state) => {
      if (kind === 'output' && !dropped && state.stored.length > 40) {
        dropped = true;
        state.stored = state.stored.subarray(0, 10);
      }
      return undefined;
    },
  });
  await agent(console);
  await console.close();
  const expected = Array.from({ length: 30 }, (_, i) => `line ${i + 1}\n`).join('');
  assert.ok(console.conflicts >= 1, 'no 409 was answered');
  assert.equal(console.stored.toString(), expected);
});

test('a cancel at the console ends the suite with 130, and its whole process group with it', async () => {
  const console = await fakeConsole({ suite: 'sleep 300 & echo $! > ../pid; echo started; wait' });
  const { dir, done } = start(console);
  const pid = await pidIn(dir);
  console.cancel = true;
  const { code } = await done;
  await console.close();
  assert.equal(code, 0);
  assert.equal(console.ends[0].exit, 130);
  assert.equal(console.ends[0].reason, 'cancelled');
  assert.equal(console.ends[0].tail, 'started\n');
  assert.ok(await gone(pid), 'the suite\'s sleep is still there');
});

test('a suite past its time that ignores SIGTERM is killed: 124, and nothing of it is left', async () => {
  const console = await fakeConsole({ suite: "trap '' TERM; sleep 300 & echo $! > ../pid; while :; do sleep 0.05; done" });
  const began = Date.now();
  const { dir } = await agent(console, { timeoutS: 1 });
  await console.close();
  assert.equal(console.ends[0].exit, 124);
  assert.equal(console.ends[0].reason, 'timeout');
  assert.ok(Date.now() - began >= 1300, 'killed before the grace time was over');
  assert.ok(await gone(await pidIn(dir)), 'the suite\'s sleep is still there');
});

test('a bundle that is not the one its sha256 names fails the setup and runs nothing', async () => {
  const console = await fakeConsole({ suite: 'touch ../ran', badSha: true });
  const { dir } = await agent(console);
  await console.close();
  assert.equal(console.ends.length, 1);
  assert.equal(console.ends[0].exit, null);
  assert.equal(console.ends[0].reason, 'setup');
  assert.match(console.ends[0].message, /sha256/);
  assert.equal(fs.existsSync(path.join(dir, 'ran')), false);
});

test('a setup that takes longer than its limit ends as setup', async () => {
  const console = await fakeConsole({ suite: 'touch ../ran', delayParts: 1000 });
  const { dir } = await agent(console, { knobs: { CF_RUN_SETUP_MS: '400' } });
  await console.close();
  assert.equal(console.ends[0].reason, 'setup');
  assert.match(console.ends[0].message, /setup took longer/);
  assert.equal(fs.existsSync(path.join(dir, 'ran')), false);
});

test('410 from the console stops the suite at once and says nothing more', async () => {
  let gone410 = false;
  const console = await fakeConsole({
    suite: 'sleep 300 & echo $! > ../pid; echo started; wait',
    fault: (kind, state) => (kind === 'output' && state.stored.length > 0 ? ((gone410 = true), 410) : undefined),
  });
  const { code, dir } = await agent(console);
  await console.close();
  assert.ok(gone410);
  assert.equal(code, 3);
  assert.deepEqual(console.ends, []);
  assert.ok(await gone(await pidIn(dir)), 'the suite\'s sleep is still there');
});

test('bytes that are not UTF-8 reach the console as U+FFFD, and the output stays whole around them', async () => {
  const console = await fakeConsole({ suite: "printf 'caf\\xe9 \\xff\\n'; printf '\\xe2\\x82'; sleep 0.3; echo done" });
  await agent(console);
  await console.close();
  assert.equal(console.invalid, 0, 'something not UTF-8 was sent');
  assert.equal(console.stored.toString(), 'caf\uFFFD \uFFFD\n\uFFFDdone\n');
  assert.equal(console.ends[0].exit, 0);
});

test('a 422 from the console skips those bytes once and the output goes on after them', async () => {
  let refused = 0;
  const console = await fakeConsole({
    suite: 'echo first; sleep 0.3; echo second; sleep 0.3; echo third',
    fault: (kind, state, body) => {
      if (kind === 'output' && body.toString().includes('second') && refused++ === 0) return 422;
      return undefined;
    },
  });
  const { code } = await agent(console);
  await console.close();
  assert.equal(code, 0);
  assert.equal(refused, 1, 'the refused bytes were sent again');
  assert.equal(console.stored.toString(), 'first\nthird\n');
  assert.ok(contiguous(console.posts), JSON.stringify(console.posts));
  assert.equal(console.ends[0].exit, 0);
});

const lines = (n) => Array.from({ length: n }, (_, i) => `line ${i + 1}\n`).join('');
const slowLines = (n) => `for i in $(seq 1 ${n}); do echo "line $i"; sleep 0.02; done`;

for (const [status, answer] of [
  [403, { status: 403, headers: { 'content-type': 'text/html' }, body: '<html>forbidden</html>' }],
  [404, 404],
  [413, 413],
  ['409 without have', { status: 409, headers: { 'content-type': 'application/json' }, body: '{}' }],
]) {
  test(`a ${status} to the output ends the output, and the end says so with the tail`, async () => {
    const console = await fakeConsole({
      suite: `${slowLines(10)}; echo last`,
      fault: (kind, state, body) => (kind === 'output' && body.length > 0 && state.stored.length > 0 ? answer : undefined),
    });
    const { code } = await agent(console);
    await console.close();
    assert.equal(code, 0);
    assert.equal(console.ends.length, 1);
    assert.equal(console.ends[0].exit, 0);
    assert.match(console.ends[0].message, /output/);
    assert.ok(console.ends[0].tail.endsWith('last\n'));
    assert.ok(console.posts.length < 40, `${console.posts.length} posts`);
  });
}

test('a console that refused the output still cancels the suite through its heartbeats', async () => {
  const console = await fakeConsole({
    suite: 'sleep 300 & echo $! > ../pid; echo started; wait',
    fault: (kind, state, body) => (kind === 'output' && body.length > 0 ? 413 : undefined),
  });
  const { dir, done } = start(console);
  const pid = await pidIn(dir);
  console.cancel = true;
  const { code } = await done;
  await console.close();
  assert.equal(code, 0);
  assert.equal(console.ends[0].exit, 130);
  assert.equal(console.ends[0].reason, 'cancelled');
  assert.match(console.ends[0].message, /413/);
  assert.equal(console.stored.length, 0);
  assert.ok(await gone(pid), 'the suite\'s sleep is still there');
});

test('a console that answers 409 at the offset the agent sends backs off and ends the output', async () => {
  const console = await fakeConsole({
    suite: slowLines(5),
    fault: (kind, state, body) =>
      kind === 'output' && body.length > 0
        ? { status: 409, headers: { 'content-type': 'application/json' }, body: JSON.stringify({ have: state.stored.length }) }
        : undefined,
  });
  const began = Date.now();
  await agent(console);
  await console.close();
  assert.equal(console.ends.length, 1);
  assert.match(console.ends[0].message, /output/);
  const conflicts = console.posts.filter((post) => post.length > 0).length;
  assert.ok(conflicts <= 12, `${conflicts} posts that met a 409`);
  assert.ok(Date.now() - began < 20_000);
});

test('a redirect or a page that is not the console\'s answer delivers nothing: the output and the end go again', async () => {
  let redirected = 0;
  let pages = 0;
  let endRedirected = false;
  const console = await fakeConsole({
    suite: slowLines(20),
    fault: (kind, state, body) => {
      if (kind === 'output' && body.length > 0 && redirected < 2) {
        redirected++;
        return { status: 302, headers: { location: '/login' } };
      }
      if (kind === 'output' && body.length > 0 && pages < 2) {
        pages++;
        return { status: 200, headers: { 'content-type': 'text/html' }, body: '<html>sign in</html>' };
      }
      if (kind === 'end' && !endRedirected) {
        endRedirected = true;
        return { status: 302, headers: { location: '/login' } };
      }
      return undefined;
    },
  });
  await agent(console);
  await console.close();
  assert.equal(redirected, 2);
  assert.equal(pages, 2);
  assert.equal(console.stored.toString(), lines(20));
  assert.equal(console.ends.length, 1, 'the end was taken as sent');
});

test('past what the console keeps, only heartbeats go, and the end\'s tail carries the end', async () => {
  const console = await fakeConsole({ suite: "head -c 5000 /dev/zero | tr '\\0' x; echo; echo END" });
  await agent(console, { knobs: { CF_RUN_KEEP_BYTES: '1000' } });
  await console.close();
  assert.equal(console.stored.toString(), 'x'.repeat(1000));
  assert.ok(console.posts.every((post) => post.offset + post.length <= 1000));
  assert.ok(console.ends[0].tail.endsWith('END\n'));
  assert.match(console.ends[0].message ?? '', /1000 bytes/);
  assert.equal(console.ends[0].exit, 0);
});

test('a full disk stops the spool, not the suite: the end says so', { skip: !fs.existsSync('/dev/full') }, async () => {
  const console = await fakeConsole({ suite: 'echo one; echo two; touch ../ran' });
  const { code, dir } = await agent(console, {
    prepare: (dir) => {
      fs.mkdirSync(path.join(dir, '.cf-test-run'));
      fs.symlinkSync('/dev/full', path.join(dir, '.cf-test-run', 'output.log'));
    },
  });
  await console.close();
  assert.equal(code, 0);
  assert.ok(fs.existsSync(path.join(dir, 'ran')), 'the suite did not run to its end');
  assert.equal(console.ends.length, 1);
  assert.equal(console.ends[0].exit, 0);
  assert.match(console.ends[0].message ?? '', /not kept/);
});
