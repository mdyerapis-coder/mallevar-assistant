import { afterEach, describe, expect, it } from 'bun:test';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { CodexAdapter } from '../src/harness/codex';
import type { NativePermissionHost } from '../src/harness/native/host';
import type { CoreEvent, OpenSessionInput, TurnInput, WorkerEvent } from '../../contracts/src/generated';

/**
 * Native Codex app-server boundary tests.
 *
 * Each case is driven by a real executable double (`codex-double.mjs`) written
 * into a temp dir and passed as the adapter's `binary`. The double records its
 * argv, logs every inbound JSON-RPC frame and writes phase markers, so the tests
 * assert on what the real process did, not on a mock's call list.
 *
 * Timing: the double is a real child process, so fake timers cannot control it
 * (the clock being exercised lives in another OS process). Waits here are either
 * bounded polls on a real signal file (`waitForPhase`) or short deliberate
 * windows around that process's own behaviour, not guesses at a duration.
 */

const tempDirs: string[] = [];
afterEach(() => {
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function makeDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  tempDirs.push(dir);
  return dir;
}

/** A host that never silently allows: `prepareAction` is the core's decision. */
const host: NativePermissionHost = {
  async state() { return { policy_version: "1", state_digest: 'd'.repeat(64) }; },
  async prepareAction() { throw new Error('CoreDeniedUnknownPermission'); },
  async verifyAnswer() { return false; },
  async recordNativeEvent() { /* noop */ },
};

function sessionInput(cwd: string, overrides: Partial<OpenSessionInput> = {}): OpenSessionInput {
  return {
    run_id: 'run-codex-1',
    project_id: 'proj-codex',
    cwd,
    route: { connection_id: 'local', model_id: 'gpt-5.2-codex', harness: 'codex' },
    context: { digest: 'd', brief: '', constraints: [], excerpts: [], artifacts: [], unresolved_questions: [], originating_sessions: [] },
    policy: {
      version: "1",
      project_id: 'proj-codex',
      allowed_connection_ids: ['local'],
      allowed_tools: ['read'],
      read_roots: [cwd],
      write_roots: [cwd],
      network_destinations: [],
      local_only: true,
      max_workers: 1,
      max_requests: 4,
      max_cost_microusd: "0",
      spend_policy: 'free_only',
      allow_unknown_cost: false,
    },
    ...overrides,
  };
}

function turnInput(session: OpenSessionInput): TurnInput {
  return {
    request_id: 'req-1',
    message_id: 'msg-1',
    content: [{ type: 'text', text: 'ping' }],
    allowed_tools: [{ name: 'read', description: '', input_schema: {} }],
    budget: session.policy,
  };
}

function turnFinished(events: WorkerEvent[]): Array<Extract<CoreEvent, { type: 'turn_finished' }>> {
  return events
    .map(event => event.payload)
    .filter((payload): payload is Extract<CoreEvent, { type: 'turn_finished' }> => payload.type === 'turn_finished');
}

function readLines(path: string): string[] {
  if (!existsSync(path)) return [];
  return readFileSync(path, 'utf8').split('\n').filter(line => line.length > 0);
}

function readFrames(path: string): Array<Record<string, unknown>> {
  return readLines(path).map(line => JSON.parse(line) as Record<string, unknown>);
}

async function waitForPhase(dir: string, name: string, timeoutMs = 5_000): Promise<void> {
  const path = join(dir, 'phase.log');
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (readLines(path).includes(name)) return;
    await Bun.sleep(20);
  }
  throw new Error(`double phase timeout: ${name}`);
}

/**
 * Write a real executable Codex app-server double. It answers `--version`, then
 * speaks newline-delimited JSON-RPC on stdio, logging argv, inbound frames and
 * phase markers into its own directory.
 */
function writeDouble(dir: string, mode: 'resume' | 'unknown-request' | 'late-failure' | 'cancel'): string {
  const path = join(dir, 'codex-double.mjs');
  const source = '#!' + process.execPath + '\n' + String.raw`import { appendFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const DIR = dirname(fileURLToPath(import.meta.url));
const MODE = ${JSON.stringify(mode)};
const argvLog = join(DIR, 'argv.log');
const rxLog = join(DIR, 'rx.log');
const phaseLog = join(DIR, 'phase.log');

const phase = (name) => appendFileSync(phaseLog, name + '\n');

appendFileSync(argvLog, JSON.stringify(process.argv.slice(1)) + '\n');

if (process.argv.includes('--version')) {
  process.stdout.write('codex-cli 0.162.1\n');
  process.exit(0);
}

process.on('exit', () => phase('exited'));

const threadId = 'thread-1';
const turnId = 'turn-1';
const send = (frame) => process.stdout.write(JSON.stringify(frame) + '\n');

function runTurn() {
  send({ jsonrpc: '2.0', method: 'turn/started', params: { threadId, turnId } });
  phase('turn-started');
  if (MODE === 'unknown-request') {
    send({ jsonrpc: '2.0', id: 777, method: 'vendor/unrecognised', params: { note: 'not an approval method' } });
    setTimeout(() => send({ jsonrpc: '2.0', method: 'turn/completed', params: { threadId, turnId, turn: { id: turnId } } }), 200);
    return;
  }
  if (MODE === 'late-failure') {
    setTimeout(() => {
      send({ jsonrpc: '2.0', method: 'item/completed', params: { threadId, turnId, item: { id: 'tool-in-turn', type: 'commandExecution', status: 'failed', command: 'false', exitCode: 1 } } });
      send({ jsonrpc: '2.0', method: 'turn/completed', params: { threadId, turnId, turn: { id: turnId } } });
      phase('turn-completed');
      setTimeout(() => {
        send({ jsonrpc: '2.0', method: 'item/completed', params: { threadId, turnId, item: { id: 'tool-late', type: 'fileChange', status: 'failed' } } });
        phase('late-failure-emitted');
      }, 150);
    }, 50);
  }
}

function onRequest(frame) {
  const id = frame.id;
  const method = frame.method;
  const params = frame.params || {};
  if (method === 'initialize') { send({ jsonrpc: '2.0', id, result: { ok: true } }); return; }
  if (method === 'thread/start') { send({ jsonrpc: '2.0', id, result: { thread: { id: threadId, cwd: params.cwd } } }); phase('opened'); return; }
  if (method === 'thread/resume') { send({ jsonrpc: '2.0', id, result: { thread: { id: params.threadId, cwd: params.cwd } } }); phase('resumed'); return; }
  if (method === 'turn/start') { send({ jsonrpc: '2.0', id, result: { ok: true } }); setTimeout(runTurn, 5); return; }
  if (method === 'turn/interrupt') {
    send({ jsonrpc: '2.0', id, result: { ok: true } });
    phase('interrupt-acked');
    if (MODE === 'cancel') {
      setTimeout(() => {
        send({ jsonrpc: '2.0', method: 'turn/completed', params: { threadId, turnId, turn: { id: turnId, status: 'interrupted' } } });
        phase('stopping');
        setTimeout(() => process.exit(0), 20);
      }, 500);
    }
    return;
  }
  send({ jsonrpc: '2.0', id, result: { ok: true } });
}

let buffer = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf('\n')) !== -1) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (!line.trim()) continue;
    let frame;
    try { frame = JSON.parse(line); } catch { continue; }
    appendFileSync(rxLog, JSON.stringify(frame) + '\n');
    if (frame && typeof frame === 'object' && frame.method) onRequest(frame);
    else if (frame && frame.id === 777) phase('deny-observed');
  }
});
process.stdin.on('end', () => process.exit(0));
`;
  writeFileSync(path, source, { mode: 0o755 });
  chmodSync(path, 0o755);
  return path;
}

describe('codex native session boundary', () => {
  it('resume_rejects_project_or_route_mismatch', async () => {
    const dir = makeDir('codex-resume-');
    const binary = writeDouble(dir, 'resume');

    // A resume that borrows another project's/route's session must be refused.
    const borrowed = {
      harness: 'codex' as const,
      native_id: 'borrowed-thread',
      native_version: '0',
      project_id: 'another-project',
      cwd: join(dir, 'elsewhere'),
      resolved_route: { connection_id: 'other-connection', model_id: 'other-model' },
    };
    const mismatchCwd = join(dir, 'work-mismatch');
    mkdirSync(mismatchCwd, { recursive: true });
    const mismatchAdapter = new CodexAdapter(host, { binary, home: join(dir, 'home-mismatch') });

    await expect(mismatchAdapter.open(sessionInput(mismatchCwd, { resume: borrowed }))).rejects.toThrow('SessionIdentityMismatch');

    // No server was ever asked to resume the borrowed session: the only argv the
    // double observed is the version probe, and it received no request frames.
    const argv = readLines(join(dir, 'argv.log')).join('\n');
    expect(argv).toContain('--version');
    expect(argv).not.toContain('app-server');
    expect(readFrames(join(dir, 'rx.log')).length).toBe(0);

    // Positive control: a matching resume does reach the server and keeps the
    // borrowed native identity, so the refusal above is a real mismatch check.
    const resumeCwd = join(dir, 'work-resume');
    mkdirSync(resumeCwd, { recursive: true });
    const base = sessionInput(resumeCwd);
    const matchingInput: OpenSessionInput = {
      ...base,
      resume: { harness: 'codex', native_id: 'thread-abc', native_version: '0', project_id: base.project_id, cwd: base.cwd, resolved_route: base.route },
    };
    const resumeAdapter = new CodexAdapter(host, { binary, home: join(dir, 'home-resume') });
    const ref = await resumeAdapter.open(matchingInput);
    try {
      expect(ref.native_id).toBe('thread-abc');
      const requests = readFrames(join(dir, 'rx.log')).filter(frame => typeof frame['method'] === 'string');
      expect(requests.some(frame => frame['method'] === 'thread/resume' && (frame['params'] as Record<string, unknown>)?.['threadId'] === 'thread-abc')).toBe(true);
    } finally {
      await resumeAdapter.close(ref).catch(() => undefined);
    }
  });

  it('unknown_server_request_is_denied', async () => {
    const dir = makeDir('codex-unknown-');
    const binary = writeDouble(dir, 'unknown-request');
    const cwd = join(dir, 'work');
    mkdirSync(cwd, { recursive: true });
    const adapter = new CodexAdapter(host, { binary, home: join(dir, 'home') });
    const session = sessionInput(cwd);
    const events: WorkerEvent[] = [];
    const ref = await adapter.open(session);
    try {
      for await (const event of adapter.turn(ref, turnInput(session))) events.push(event);

      // The double answers server requests with a number-id result frame; wait
      // until it has observed our answer for its unrecognised request.
      await waitForPhase(dir, 'deny-observed');
      const answer = readFrames(join(dir, 'rx.log')).find(frame => frame['id'] === 777 && typeof frame['method'] !== 'string');
      expect(answer).toBeDefined();
      expect(answer?.['result']).toEqual({ denied: true });

      // The deny did not fail the turn; a denied unknown request is not a turn error.
      const finished = turnFinished(events);
      expect(finished.length).toBe(1);
      expect(finished[0]?.status).toBe('succeeded');
    } finally {
      await adapter.close(ref).catch(() => undefined);
    }
  });

  it('late_tool_failure_is_not_success', async () => {
    const dir = makeDir('codex-late-');
    const binary = writeDouble(dir, 'late-failure');
    const cwd = join(dir, 'work');
    mkdirSync(cwd, { recursive: true });
    const adapter = new CodexAdapter(host, { binary, home: join(dir, 'home') });
    const session = sessionInput(cwd);
    const events: WorkerEvent[] = [];
    const ref = await adapter.open(session);
    try {
      for await (const event of adapter.turn(ref, turnInput(session))) events.push(event);

      // The native stream carried a failed tool (both inside the turn and late),
      // and then closed. A turn whose stream carries a tool failure must never be
      // translated into a successful turn for the core.
      const finished = turnFinished(events);
      expect(finished.length).toBeGreaterThan(0);
      expect(finished.every(payload => payload.status !== 'succeeded')).toBe(true);
    } finally {
      await adapter.close(ref).catch(() => undefined);
    }
  });

  it('native_cancel_waits_for_observed_stop', async () => {
    const dir = makeDir('codex-cancel-');
    const binary = writeDouble(dir, 'cancel');
    const cwd = join(dir, 'work');
    mkdirSync(cwd, { recursive: true });
    const adapter = new CodexAdapter(host, { binary, home: join(dir, 'home') });
    const session = sessionInput(cwd);
    const events: WorkerEvent[] = [];
    let turnDone: Promise<void> | undefined;
    const ref = await adapter.open(session);
    try {
      turnDone = (async () => {
        for await (const event of adapter.turn(ref, turnInput(session))) events.push(event);
      })();
      await waitForPhase(dir, 'turn-started');
      // The adapter exposes no observable signal for "active turn bound", so give
      // it a short real window to consume the native turn/started frame; if it had
      // not bound the turn, the interrupt below would be refused and the
      // `interrupt-acked` phase wait would fail loudly rather than pass silently.
      await Bun.sleep(150);

      let settled: 'resolved' | 'rejected' | null = null;
      const interrupted = adapter.interrupt(ref).then(
        () => { settled = 'resolved'; return 'resolved' as const; },
        () => { settled = 'rejected'; return 'rejected' as const; },
      );

      await waitForPhase(dir, 'interrupt-acked');
      // Deliberate window: the double acknowledged the cancel but keeps running
      // for ~500ms, so this checks interrupt() against a genuinely live process.
      await Bun.sleep(150);

      // No stop receipt may be produced while the native process is still alive.
      expect(settled).not.toBe('resolved');

      await waitForPhase(dir, 'exited', 10_000);
      expect(await interrupted).toBe('resolved');
    } finally {
      await adapter.close(ref).catch(() => undefined);
      await turnDone?.catch(() => undefined);
    }
  });
});
