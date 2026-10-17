// The test run's agent: the workload of a `test-run` machine, run by the image's Node as the image's user `runner`.
//
// It talks to the console by itself, at CF_RUN_URL with the run's token CF_RUN_TOKEN (Bearer), and to nothing else:
//
// - Setup, ten minutes at most: `GET <url>/spec` says the commit, the suite, an image or none, and the git bundles;
//   each bundle comes in parts (`GET <url>/bundles/<i>/parts/<j>`) and is checked against its sha256 and size. Then
//   `git init`, each bundle fetched in order into `refs/cf-bundle-<i>/*` (each is a prerequisite of the next), and the
//   commit checked out.
// - The suite: `bash -lc <suite>` in a process group of its own, or in `docker run` when the spec names an image, with
//   the machine's environment less CF_RUN_* and with CI=true and CF_TEST_REMOTE=1. Its output, standard output and
//   error as one stream, goes to a file first, so a console that does not answer never holds it up, and from there
//   to `POST <url>/output?offset=<n>` by byte offset, 256 KiB at most at a time, cut where no UTF-8 character is
//   split. An empty post is the heartbeat, and its answer carries the console's cancel.
// - The end: once the console has all the output, `POST <url>/end` with the exit, why it ended and the last 64 KiB.
//   The suite's time over: SIGTERM to its group (and `docker kill`), SIGKILL 20 s later, exit 124. Cancelled: the
//   same, exit 130. Killed by a signal: 128 and its number.
//
// A network error, a 429 and a 5xx are tried again, 1 s and twice as long each time up to 30 s. 409 says the console
// has another offset: the output goes on from there. 401 and 410 say the run is over at the console: the suite is
// killed and the agent ends at once, saying nothing more.
//
// Node's own modules and fetch only: the image has Node 24 and no packages for this. The `CF_RUN_*_MS` knobs and
// CF_RUN_DIR are for the agent's own tests; the Runner gives the machine none of them.

import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import { open, mkdir, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';

const env = process.env;
const knob = (name, fallback) => {
  const value = Number(env[name]);
  return Number.isFinite(value) && value > 0 ? value : fallback;
};

const RUN_URL = (env.CF_RUN_URL || '').replace(/\/+$/, '');
const TOKEN = env.CF_RUN_TOKEN || '';
const RUN = RUN_URL.split('/').pop() || 'test-run';
const SETUP_MS = knob('CF_RUN_SETUP_MS', 10 * 60_000);
const HEARTBEAT_MS = knob('CF_RUN_HEARTBEAT_MS', 10_000);
const GRACE_MS = knob('CF_RUN_KILL_GRACE_MS', 20_000);
const BACKOFF_MIN_MS = knob('CF_RUN_BACKOFF_MIN_MS', 1_000);
const BACKOFF_MAX_MS = knob('CF_RUN_BACKOFF_MAX_MS', 30_000);
const POLL_MS = knob('CF_RUN_POLL_MS', 250);
const REQUEST_MS = knob('CF_RUN_REQUEST_MS', 60_000);
const CHUNK = 256 * 1024;
const TAIL = 64 * 1024;
const HOME = env.CF_RUN_DIR || env.HOME || os.homedir();
const OWN = path.join(HOME, '.cf-test-run');
const WORK = path.join(HOME, 'work');
const SPOOL = path.join(OWN, 'output.log');

const log = (...words) => console.error('[test-run]', ...words);

/** The run is over at the console (401, 410): nothing more is said to it. */
class Gone extends Error {}
/** Setup failed: the end says so, with this message. */
class SetupError extends Error {}

/** Waits `ms`, or less when `signal` aborts. */
const pause = (ms, signal) => sleep(ms, undefined, { signal }).catch(() => {});

/**
 * One request to the console, tried again on a network error, 429 and 5xx with a backoff, until it gets another
 * answer or `signal` aborts. 401 and 410 throw `Gone`.
 */
async function call(method, suffix, { body, type, signal, timeoutMs = REQUEST_MS } = {}) {
  let wait = BACKOFF_MIN_MS;
  for (;;) {
    if (signal?.aborted) throw signal.reason;
    let response;
    try {
      const signals = [AbortSignal.timeout(timeoutMs)];
      if (signal) signals.push(signal);
      response = await fetch(RUN_URL + suffix, {
        method,
        headers: { authorization: `Bearer ${TOKEN}`, ...(type ? { 'content-type': type } : {}) },
        body,
        signal: AbortSignal.any(signals),
      });
    } catch (error) {
      if (signal?.aborted) throw signal.reason;
      log(`${method} ${suffix}: ${error.cause?.code || error.name}; again in ${wait} ms`);
      await pause(wait, signal);
      wait = Math.min(wait * 2, BACKOFF_MAX_MS);
      continue;
    }
    if (response.status === 401 || response.status === 410) {
      await response.body?.cancel();
      throw new Gone(`the console answered ${response.status}: the run is over there`);
    }
    if (response.status === 429 || response.status >= 500) {
      await response.body?.cancel();
      log(`${method} ${suffix}: ${response.status}; again in ${wait} ms`);
      await pause(wait, signal);
      wait = Math.min(wait * 2, BACKOFF_MAX_MS);
      continue;
    }
    return response;
  }
}

async function json(response) {
  try {
    return await response.json();
  } catch {
    return {};
  }
}

/** Where `bytes` may be cut without splitting a UTF-8 character: its length, or the start of a character cut short. */
function boundary(bytes) {
  const end = bytes.length;
  for (let i = end - 1; i >= Math.max(0, end - 4); i--) {
    const byte = bytes[i];
    if ((byte & 0xc0) === 0x80) continue;
    const length = byte >= 0xf0 ? 4 : byte >= 0xe0 ? 3 : byte >= 0xc0 ? 2 : 1;
    return i + length > end ? i : end;
  }
  return end;
}

/**
 * Sends the spooled output to the console from the offset it has, as it grows, and a heartbeat while it does not. The
 * console's cancel calls `onCancel` once; `Gone` calls `onGone`.
 */
class Sender {
  constructor(onCancel, onGone) {
    this.onCancel = onCancel;
    this.onGone = onGone;
    this.sent = 0;
    this.dropped = 0;
    this.last = 0;
    this.final = false;
    this.stopped = new AbortController();
    this.wake = null;
    this.done = null;
    this.cancelled = false;
  }

  start() {
    this.running = this.loop().catch((error) => {
      if (error instanceof Gone) return this.onGone(error);
      if (!this.stopped.signal.aborted) log(`the output stopped: ${error.stack || error}`);
    });
  }

  /** Waits until the console has every byte of the output, the last ones too, and stops. */
  async drain() {
    this.final = true;
    const done = new Promise((resolve) => (this.done = resolve));
    this.wake?.();
    await Promise.race([done, this.running]);
    this.stopped.abort(new Error('stopped'));
    await this.running;
  }

  async loop() {
    const file = await open(SPOOL, 'r');
    try {
      for (;;) {
        const final = this.final;
        const size = (await file.stat()).size;
        let length = Math.max(0, Math.min(size - this.sent, CHUNK));
        let body = Buffer.alloc(0);
        if (length > 0) {
          body = Buffer.alloc(length);
          const { bytesRead } = await file.read(body, 0, length, this.sent);
          body = body.subarray(0, bytesRead);
          // A character cut short waits for its last bytes, unless nothing more can come.
          if (!(final && this.sent + body.length === size)) body = body.subarray(0, boundary(body));
          length = body.length;
        }
        const heartbeat = Date.now() - this.last >= HEARTBEAT_MS;
        if (length > 0 || (heartbeat && !final)) {
          await this.post(body);
          continue;
        }
        if (final && this.sent >= size) {
          this.done?.();
          return;
        }
        await new Promise((resolve) => {
          this.wake = resolve;
          setTimeout(resolve, POLL_MS);
        });
        this.wake = null;
      }
    } finally {
      await file.close();
    }
  }

  async post(body) {
    // The console counts the bytes it took; the spool also holds those it refused (`dropped`).
    const offset = this.sent - this.dropped;
    const response = await call('POST', `/output?offset=${offset}`, {
      body,
      type: 'text/plain; charset=utf-8',
      signal: this.stopped.signal,
    });
    const answer = await json(response);
    const have = Number.isSafeInteger(answer.have) && answer.have >= 0 ? answer.have : null;
    if (response.status === 409 && have !== null) {
      log(`the console has ${have} bytes, not ${offset}: from there`);
      this.sent = have + this.dropped;
      return;
    }
    if (response.status === 422 && body.length > 0) {
      // The console found no UTF-8 in what the agent decoded as UTF-8: a mistake of the agent's or the console's.
      // Those bytes are given up, never sent again, and the next post (a heartbeat if nothing else) says where the
      // console is.
      log(`POST /output: 422 for ${body.length} bytes at ${offset}; they are skipped`);
      this.dropped += body.length;
      this.sent += body.length;
      this.last = 0;
      return;
    }
    if (!response.ok) {
      log(`POST /output: ${response.status}; again in ${BACKOFF_MIN_MS} ms`);
      await pause(BACKOFF_MIN_MS, this.stopped.signal);
      return;
    }
    this.last = Date.now();
    this.sent = have !== null ? have + this.dropped : this.sent + body.length;
    if (answer.cancel === true && !this.cancelled) {
      this.cancelled = true;
      this.onCancel();
    }
  }
}

/** Runs a program to its end; its standard error, the last 2 KiB, comes with a failure. */
function run(program, args, { cwd, signal } = {}) {
  return new Promise((resolve, reject) => {
    let stderr = '';
    const child = spawn(program, args, { cwd, signal, stdio: ['ignore', 'ignore', 'pipe'] });
    child.stderr.on('data', (data) => (stderr = (stderr + data).slice(-2048)));
    child.on('error', (error) => reject(signal?.aborted ? signal.reason : new SetupError(`${program}: ${error.message}`)));
    child.on('close', (code, sig) => {
      if (code === 0) return resolve();
      if (signal?.aborted) return reject(signal.reason);
      reject(new SetupError(`${program} ${args[0]} ended with ${code ?? sig}: ${stderr.trim()}`));
    });
  });
}

function check(spec) {
  const hex = (text, lengths) => typeof text === 'string' && lengths.includes(text.length) && /^[0-9a-f]+$/.test(text);
  if (!hex(spec?.commit, [40, 64])) throw new SetupError('the spec names no commit');
  if (typeof spec.suite !== 'string' || spec.suite.trim() === '') throw new SetupError('the spec names no suite');
  if (spec.image != null && (typeof spec.image !== 'string' || spec.image === '' || spec.image.startsWith('-'))) {
    throw new SetupError('the spec names an image that is not one');
  }
  if (!Number.isSafeInteger(spec.partBytes) || spec.partBytes <= 0) throw new SetupError('the spec has no part size');
  if (!Array.isArray(spec.bundles) || spec.bundles.length === 0) throw new SetupError('the spec has no bundle');
  for (const bundle of spec.bundles) {
    if (!hex(bundle?.sha256, [64]) || !Number.isSafeInteger(bundle.size) || bundle.size < 0) {
      throw new SetupError('a bundle of the spec has no sha256 or size');
    }
    if (!Number.isSafeInteger(bundle.parts) || bundle.parts < 1) throw new SetupError('a bundle of the spec has no parts');
  }
  return spec;
}

/** Fetches the spec and the bundles and checks the commit out in WORK. Says the spec. */
async function setup(signal) {
  const response = await call('GET', '/spec', { signal });
  if (!response.ok) throw new SetupError(`the console answered ${response.status} to the spec`);
  const spec = check(await json(response));
  await rm(WORK, { recursive: true, force: true });
  await mkdir(WORK, { recursive: true });
  await run('git', ['init', '-q', WORK], { signal });
  for (const [i, bundle] of spec.bundles.entries()) {
    const file = path.join(OWN, `bundle-${i}`);
    const out = await open(file, 'w');
    const hash = createHash('sha256');
    let size = 0;
    try {
      for (let j = 0; j < bundle.parts; j++) {
        const part = await call('GET', `/bundles/${i}/parts/${j}`, { signal, timeoutMs: 10 * REQUEST_MS });
        if (!part.ok) throw new SetupError(`the console answered ${part.status} to part ${j} of bundle ${i}`);
        const bytes = Buffer.from(await part.arrayBuffer());
        size += bytes.length;
        if (size > bundle.size) throw new SetupError(`bundle ${i} is larger than its ${bundle.size} bytes`);
        hash.update(bytes);
        await out.write(bytes);
      }
    } finally {
      await out.close();
    }
    if (size !== bundle.size) throw new SetupError(`bundle ${i} has ${size} bytes, not ${bundle.size}`);
    if (hash.digest('hex') !== bundle.sha256) throw new SetupError(`bundle ${i} is not the one its sha256 names`);
    await run('git', ['-C', WORK, 'fetch', '-q', file, `+refs/*:refs/cf-bundle-${i}/*`], { signal });
    await rm(file, { force: true });
  }
  await run('git', ['-C', WORK, '-c', 'advice.detachedHead=false', 'checkout', '-q', '--detach', spec.commit], {
    signal,
  });
  return spec;
}

/** The last 64 KiB of the output, from the start of a character. */
function tail() {
  const size = fs.statSync(SPOOL).size;
  const start = Math.max(0, size - TAIL);
  const bytes = Buffer.alloc(size - start);
  const fd = fs.openSync(SPOOL, 'r');
  try {
    fs.readSync(fd, bytes, 0, bytes.length, start);
  } finally {
    fs.closeSync(fd);
  }
  let from = 0;
  while (start > 0 && from < bytes.length && from < 4 && (bytes[from] & 0xc0) === 0x80) from++;
  return bytes.subarray(from).toString('utf8');
}

/** Runs the suite until it ends, its time is over or the console cancels it. Says `{ exit, reason, message }`. */
function suite(spec, timeoutS, sender) {
  const suiteEnv = Object.fromEntries(Object.entries(env).filter(([name]) => !name.startsWith('CF_RUN_')));
  Object.assign(suiteEnv, { CI: 'true', CF_TEST_REMOTE: '1' });
  const [program, args] = spec.image
    ? ['docker', ['run', '--rm', '--init', '--name', RUN, '-e', 'CI=true', '-e', 'CF_TEST_REMOTE=1',
        '-v', `${WORK}:/work`, '-w', '/work', spec.image, 'bash', '-lc', spec.suite]]
    : ['bash', ['-lc', spec.suite]];
  // Standard error joins standard output before the suite starts, so both are one stream in the order written. The
  // agent reads it and spools it as UTF-8: bytes that are none become U+FFFD, and a character the suite writes in two
  // goes to the spool whole, so the console only ever gets UTF-8.
  const child = spawn('/bin/sh', ['-c', 'exec "$@" 2>&1', 'sh', program, ...args], {
    cwd: WORK,
    env: suiteEnv,
    detached: true,
    stdio: ['ignore', 'pipe', 'ignore'],
  });
  const output = fs.openSync(SPOOL, 'a');
  const decoder = new TextDecoder('utf-8', { ignoreBOM: true });
  const spool = (text) => text && fs.writeSync(output, text);
  child.stdout?.on('data', (bytes) => spool(decoder.decode(bytes, { stream: true })));
  // A process that left the suite's group may hold the pipe: it is not waited for long.
  const drained = new Promise((resolve) => {
    if (!child.stdout) return resolve();
    child.stdout.on('close', resolve);
    child.stdout.on('error', resolve);
  });
  const group = (signal) => {
    try {
      process.kill(-child.pid, signal);
    } catch {}
  };
  const docker = (signal) => {
    if (!spec.image) return;
    spawn('docker', ['kill', '--signal', signal, RUN], { stdio: 'ignore' }).on('error', () => {});
  };
  let stopping = null;
  let killer = null;
  const stop = (why) => {
    if (stopping) return;
    stopping = why;
    log(`the suite is stopped: ${why}`);
    group('SIGTERM');
    docker('TERM');
    killer = setTimeout(() => {
      group('SIGKILL');
      docker('KILL');
    }, GRACE_MS);
  };
  sender.onCancel = () => stop('cancelled');
  sender.onGone = (error) => {
    log(error.message);
    group('SIGKILL');
    docker('KILL');
    process.exit(3);
  };
  const timer = setTimeout(() => stop('timeout'), timeoutS * 1000);
  // A cancel that came between the setup and here.
  if (sender.cancelled) stop('cancelled');
  return new Promise((resolve) => {
    let ended = false;
    const end = async (code, signal, error) => {
      if (ended) return;
      ended = true;
      clearTimeout(timer);
      clearTimeout(killer);
      // What the suite left running in its group goes with it.
      group('SIGKILL');
      await Promise.race([drained, sleep(5_000)]);
      child.stdout?.destroy();
      spool(decoder.decode());
      fs.closeSync(output);
      if (spec.image) spawn('docker', ['rm', '-f', RUN], { stdio: 'ignore' }).on('error', () => {});
      if (stopping === 'timeout') {
        return resolve({ exit: 124, reason: 'timeout', message: `the suite ran past its ${timeoutS} s` });
      }
      if (stopping === 'cancelled') return resolve({ exit: 130, reason: 'cancelled', message: 'cancelled at the console' });
      if (error) return resolve({ exit: null, reason: 'setup', message: `${program} did not start: ${error.message}` });
      const exit = code ?? 128 + (os.constants.signals[signal] ?? 0);
      resolve({ exit, reason: 'exited', message: signal ? `the suite was killed by ${signal}` : undefined });
    };
    child.on('error', (error) => end(null, null, error));
    child.on('exit', (code, signal) => end(code, signal));
  });
}

async function main() {
  if (!RUN_URL || !TOKEN) {
    log('CF_RUN_URL and CF_RUN_TOKEN are not set');
    process.exit(2);
  }
  await mkdir(OWN, { recursive: true });
  await writeFile(SPOOL, '');
  const setupStop = new AbortController();
  const gone = (error) => {
    log(error.message);
    process.exit(3);
  };
  const sender = new Sender(() => setupStop.abort(new SetupError('cancelled at the console during setup')), gone);
  sender.start();

  let outcome;
  let spec = null;
  const limit = setTimeout(() => setupStop.abort(new SetupError(`setup took longer than ${SETUP_MS / 1000} s`)), SETUP_MS);
  try {
    spec = await setup(setupStop.signal);
  } catch (error) {
    if (error instanceof Gone) gone(error);
    const message = error instanceof SetupError ? error.message : `setup failed: ${error.message || error}`;
    const cancelled = sender.cancelled;
    outcome = { exit: null, reason: cancelled ? 'cancelled' : 'setup', message: cancelled ? 'cancelled at the console' : message };
  } finally {
    clearTimeout(limit);
  }
  if (spec) {
    const fromEnv = Number(env.CF_RUN_TIMEOUT_S);
    const timeouts = [fromEnv, spec.timeoutS].filter((n) => Number.isFinite(n) && n > 0);
    const timeoutS = timeouts.length ? Math.min(...timeouts) : 1800;
    log(`setup done: ${spec.commit}; the suite runs for ${timeoutS} s at most`);
    outcome = await suite(spec, timeoutS, sender);
  }
  await sender.drain();
  const body = JSON.stringify({ ...outcome, tail: tail() });
  const response = await call('POST', '/end', { body, type: 'application/json' }).catch((error) => {
    if (error instanceof Gone) gone(error);
    throw error;
  });
  if (!response.ok) log(`POST /end: ${response.status}; the console did not take the end`);
  log(`ended: ${outcome.reason} ${outcome.exit ?? ''}`);
  process.exit(0);
}

main().catch((error) => {
  log(`the agent failed: ${error.stack || error}`);
  process.exit(1);
});
