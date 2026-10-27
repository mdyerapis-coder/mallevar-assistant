import { afterEach, describe, expect, it } from 'bun:test';
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, watch, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { OpenCodeAdapter } from '../src/harness/opencode';
import { canonical, type NativePermissionHost } from '../src/harness/native/host';
import type { HarnessKind, OpenSessionInput, SessionRef, TurnInput, WorkerEvent } from '../../contracts/src/generated';

// A genuine process double: a real executable the adapter spawns that speaks just
// enough newline-delimited ACP JSON-RPC to reach the permission round-trip. It
// records its own argv and stdio to files, so the assertions below observe the
// process's real bytes rather than mock-internal call counts.
const DOUBLE_SOURCE = `#!/usr/bin/env bun
import { appendFileSync, existsSync, readFileSync, watch, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import readline from 'node:readline';

const dir = dirname(fileURLToPath(import.meta.url));
const logPath = join(dir, 'double-log.txt');
const deniedPath = join(dir, 'denied-file');
const log = (line) => appendFileSync(logPath, line + '\\n');

const config = JSON.parse(readFileSync(join(dir, 'double-config.json'), 'utf8'));
const terminalResult = config.terminalReason === null ? {} : { stopReason: config.terminalReason ?? 'end_turn' };
const argv = process.argv.slice(2);
log('INVOKE ' + argv.join(' '));

if (argv.includes('--version')) {
  process.stdout.write(config.version + '\\n');
  process.exit(0);
}
if (!argv.includes('acp')) process.exit(0);

const nativeId = config.nativeId ?? 'native-session-1';
const emit = (obj) => process.stdout.write(JSON.stringify(obj) + '\\n');
let promptId = null;
const permissionId = 900;

const rl = readline.createInterface({ input: process.stdin });
rl.on('close', () => process.exit(0));
rl.on('line', (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let msg;
  try { msg = JSON.parse(trimmed); } catch { return; }
  if (msg.id !== undefined && msg.method === undefined) {
    if (msg.id === permissionId) {
      const outcome = msg.result?.outcome;
      const selected = outcome?.outcome === 'selected' ? outcome.optionId : undefined;
      log('PERMISSION_RESULT ' + (outcome?.outcome ?? 'cancelled') + ':' + (selected ?? 'none'));
      if (selected === 'allow-once') {
        writeFileSync(deniedPath, 'tool-executed');
        log('TOOL_EXECUTED');
      } else {
        log('TOOL_DENIED');
      }
      if (promptId !== null) { emit({ jsonrpc: '2.0', id: promptId, result: terminalResult }); promptId = null; }
    }
    return;
  }
  switch (msg.method) {
    case 'initialize':
      emit({ jsonrpc: '2.0', id: msg.id, result: { protocolVersion: 1, agentInfo: { name: 'opencode', version: config.agentVersion ?? '0.19.0' } } });
      return;
    case 'session/new':
      log('SESSION_NEW');
      emit({ jsonrpc: '2.0', id: msg.id, result: { sessionId: nativeId, models: { availableModels: (config.models ?? []).map((m) => ({ modelId: m })) } } });
      return;
    case 'session/resume':
      log('SESSION_RESUME ' + (msg.params?.sessionId ?? ''));
      emit({ jsonrpc: '2.0', id: msg.id, result: { sessionId: msg.params?.sessionId ?? nativeId, models: { availableModels: (config.models ?? []).map((m) => ({ modelId: m })) } } });
      return;
    case 'session/prompt':
      promptId = msg.id;
      log('PROMPT_RECEIVED');
      log('PROMPT ' + JSON.stringify(msg.params?.prompt ?? null));
      if (config.streamHeld) {
        emit({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: nativeId,
          update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'partial-before-stop' } } } });
        log('UPDATE_SENT');
        return;
      }
      if (config.permission === false) { emit({ jsonrpc: '2.0', id: msg.id, result: terminalResult }); promptId = null; return; }
      log('PERMISSION_REQUEST_SENT');
      emit({ jsonrpc: '2.0', id: permissionId, method: 'session/request_permission', params: {
        sessionId: nativeId,
        toolCall: { toolCallId: 'tc-1', name: 'write', kind: 'edit', title: 'write sentinel file' },
        options: [ { optionId: 'allow-once', kind: 'allow_once', name: 'Allow once' },
          ...(config.rejectOption === false ? [] : [{ optionId: 'reject-once', kind: 'reject_once', name: 'Reject' }]) ],
      } });
      return;
    case 'session/cancel': {
      log('CANCEL_RECEIVED');
      const finish = () => {
        if (promptId !== null) { emit({ jsonrpc: '2.0', id: promptId, result: { stopReason: 'cancelled' } }); promptId = null; }
      };
      if (config.cancelAckGate && !existsSync(join(dir, 'cancel-release'))) {
        const watcher = watch(dir, () => {
          if (existsSync(join(dir, 'cancel-release'))) { watcher.close(); finish(); }
        });
        if (existsSync(join(dir, 'cancel-release'))) { watcher.close(); finish(); }
      } else finish();
      return;
    }
    default:
      if (msg.id !== undefined) emit({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'method_not_found' } });
  }
});
`;

type DoubleConfig = { version?: string; nativeId?: string; models?: string[]; permission?: boolean; agentVersion?: string;
  streamHeld?: boolean; rejectOption?: boolean; cancelAckGate?: boolean; terminalReason?: string | null };

type Double = { dir: string; binary: string; stateDir: string; deniedFile: string };

const cleanups: Array<() => void | Promise<void>> = [];
afterEach(async () => {
  // Close native processes before their temp dirs are removed.
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

function makeDouble(config: DoubleConfig = {}): Double {
  const dir = mkdtempSync(join(tmpdir(), 'ironbark-opencode-session-'));
  const binary = join(dir, 'opencode-double.mjs');
  writeFileSync(binary, DOUBLE_SOURCE);
  chmodSync(binary, 0o755);
  writeFileSync(join(dir, 'double-config.json'), JSON.stringify({
    version: 'opencode 0.19.0',
    nativeId: 'native-session-1',
    models: [],
    permission: true,
    ...config,
  }));
  cleanups.push(() => rmSync(dir, { recursive: true, force: true }));
  return { dir, binary, stateDir: join(dir, 'state'), deniedFile: join(dir, 'denied-file') };
}

function readDoubleLog(double: Double): string {
  try { return readFileSync(join(double.dir, 'double-log.txt'), 'utf8'); } catch { return ''; }
}

/** Deny-by-default host: the core refuses the permission before any approval reply is sent. */
const denyingHost: NativePermissionHost = {
  async state() { return { policy_version: "1", state_digest: 'd'.repeat(64) }; },
  async prepareAction() { throw new Error('CoreDeniedUnknownPermission'); },
  async verifyAnswer() { return false; },
  async recordNativeEvent() { /* noop */ },
};

function input(harness: HarnessKind, overrides: Partial<OpenSessionInput> = {}): OpenSessionInput {
  return {
    run_id: 'run-1',
    project_id: 'proj-1',
    cwd: '/tmp',
    route: { connection_id: 'local', model_id: 'm', harness },
    context: { digest: 'd', brief: '', constraints: [], excerpts: [], artifacts: [], unresolved_questions: [], originating_sessions: [] },
    policy: {
      version: "1",
      project_id: 'proj-1',
      allowed_connection_ids: ['local'],
      allowed_tools: ['read'],
      read_roots: ['/tmp'],
      write_roots: ['/tmp'],
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

function turn(session: OpenSessionInput): TurnInput {
  return {
    request_id: 'req-1',
    message_id: 'msg-1',
    content: [{ type: 'text', text: 'summarise the workspace' }],
    allowed_tools: [{ name: 'read', description: '', input_schema: {} }],
    budget: session.policy,
  };
}

async function runTurn(adapter: OpenCodeAdapter, ref: SessionRef, sessionInput: OpenSessionInput): Promise<WorkerEvent[]> {
  const events: WorkerEvent[] = [];
  for await (const event of adapter.turn(ref, turn(sessionInput))) events.push(event);
  return events;
}

function adapterFor(double: Double, host: NativePermissionHost = denyingHost): OpenCodeAdapter {
  return new OpenCodeAdapter(host, { binary: double.binary, stateDir: double.stateDir });
}

/** Open a session and guarantee its native process is closed after the test. */
async function openSession(adapter: OpenCodeAdapter, sessionInput: OpenSessionInput): Promise<SessionRef> {
  const ref = await adapter.open(sessionInput);
  cleanups.push(async () => { await adapter.close(ref).catch(() => undefined); });
  return ref;
}

async function waitForNativeMarker(double: Double, marker: string): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  const observe = () => {
    if (readDoubleLog(double).includes(marker)) { watcher.close(); resolve(); }
  };
  const watcher = watch(double.dir, observe);
  cleanups.push(() => watcher.close());
  observe();
  await promise;
}

describe('opencode native session', () => {
  it.each([
    { project_id: 'foreign-project' },
    { cwd: '/foreign-workspace' },
    { harness: 'hermes' as const },
    { native_version: 'unobserved-version' },
  ])('foreign_session_identity_cannot_cancel_the_owned_native_turn: %j', async changed => {
    const double = makeDouble({ streamHeld: true });
    const adapter = adapterFor(double);
    const sessionInput = input('opencode');
    const ref = await openSession(adapter, sessionInput);
    const iterator = adapter.turn(ref, turn(sessionInput))[Symbol.asyncIterator]();
    expect((await iterator.next()).value?.payload.type).toBe('turn_started');
    await expect(adapter.interrupt({ ...ref, ...changed })).rejects.toThrow('SessionIdentityMismatch');
    expect(readDoubleLog(double)).not.toContain('CANCEL_RECEIVED');
    await adapter.interrupt(ref);
    while (!(await iterator.next()).done) { /* Drain the actual terminal observation. */ }
  });

  it('mutating_the_returned_reference_cannot_rebind_native_session_authority', async () => {
    const double = makeDouble({ streamHeld: true });
    const adapter = adapterFor(double);
    const sessionInput = input('opencode');
    const ref = await openSession(adapter, sessionInput);
    const iterator = adapter.turn(ref, turn(sessionInput))[Symbol.asyncIterator]();
    expect((await iterator.next()).value?.payload.type).toBe('turn_started');
    const project = ref.project_id;
    ref.project_id = 'foreign-project';
    try {
      await expect(adapter.interrupt(ref)).rejects.toThrow('SessionIdentityMismatch');
      expect(readDoubleLog(double)).not.toContain('CANCEL_RECEIVED');
    } finally { ref.project_id = project; }
    await adapter.interrupt(ref);
    while (!(await iterator.next()).done) { /* Drain the actual terminal observation. */ }
  });

  it.each([null, 'invented_success', 'refusal', 'max_tokens', 'max_turn_requests'])('a_non_success_native_completion_never_becomes_success: %s', async terminalReason => {
    const double = makeDouble({ permission: false, terminalReason });
    const adapter = adapterFor(double);
    const sessionInput = input('opencode');
    const ref = await openSession(adapter, sessionInput);
    const terminal = (await runTurn(adapter, ref, sessionInput)).at(-1)?.payload;
    expect(terminal?.type).toBe('turn_finished');
    if (terminal?.type === 'turn_finished') expect(terminal.status).toBe('failed');
  });

  it('denial_without_a_reject_option_cancels_instead_of_selecting_allow', async () => {
    const double = makeDouble({ rejectOption: false });
    const adapter = adapterFor(double);
    const sessionInput = input('opencode');
    const ref = await openSession(adapter, sessionInput);
    await runTurn(adapter, ref, sessionInput);
    expect(existsSync(double.deniedFile)).toBe(false);
    expect(readDoubleLog(double)).toContain('PERMISSION_RESULT cancelled:none');
  });

  it('partial_output_is_observable_while_the_native_turn_is_still_running', async () => {
    const double = makeDouble({ streamHeld: true });
    const adapter = adapterFor(double);
    const sessionInput = input('opencode');
    const ref = await openSession(adapter, sessionInput);
    const iterator = adapter.turn(ref, turn(sessionInput))[Symbol.asyncIterator]();
    const first = await iterator.next();
    expect(first.value?.payload.type).toBe('turn_started');
    const partial = await iterator.next();
    expect(partial.value?.payload).toEqual({ type: 'text_delta', message_id: 'msg-1', delta: 'partial-before-stop' });
    expect(readDoubleLog(double)).not.toContain('CANCEL_RECEIVED');
    await adapter.interrupt(ref);
    const terminal = await iterator.next();
    expect(terminal.value?.payload.type).toBe('turn_finished');
    if (terminal.value?.payload.type === 'turn_finished') expect(terminal.value.payload.status).toBe('cancelled');
  });

  it('interrupt_waits_for_the_observed_native_cancel_response', async () => {
    const double = makeDouble({ streamHeld: true, cancelAckGate: true });
    const adapter = adapterFor(double);
    const sessionInput = input('opencode');
    const ref = await openSession(adapter, sessionInput);
    const events = runTurn(adapter, ref, sessionInput);
    await waitForNativeMarker(double, 'UPDATE_SENT');
    let settled = false;
    const stopping = adapter.interrupt(ref).then(() => { settled = true; });
    try {
      await waitForNativeMarker(double, 'CANCEL_RECEIVED');
      expect(settled).toBe(false);
    } finally { writeFileSync(join(double.dir, 'cancel-release'), 'release'); }
    await stopping;
    const terminal = (await events).at(-1)?.payload;
    expect(terminal?.type).toBe('turn_finished');
    if (terminal?.type === 'turn_finished') expect(terminal.status).toBe('cancelled');
    expect(readDoubleLog(double)).not.toContain('SESSION_RESUME');
  });

  it('permission_denial_precedes_tool_execution', async () => {
    const double = makeDouble();
    const adapter = adapterFor(double);
    const sessionInput = input('opencode');
    const ref = await openSession(adapter, sessionInput);

    const events = await runTurn(adapter, ref, sessionInput);

    const log = readDoubleLog(double);
    // The double actually reached the permission round-trip over stdio.
    expect(log).toContain('PROMPT_RECEIVED');
    expect(log).toContain('PERMISSION_REQUEST_SENT');
    // Real observed effect: the denied permission produced a non-allow option and no tool run.
    expect(log).toContain('TOOL_DENIED');
    expect(log).not.toContain('TOOL_EXECUTED');
    expect(log).not.toContain('PERMISSION_RESULT selected:allow-once');
    expect(existsSync(double.deniedFile)).toBe(false);

    // And the adapter emitted no tool execution for the denied call.
    expect(events.some(event => event.payload.type === 'tool_started')).toBe(false);
    const finished = events.at(-1)?.payload;
    expect(finished?.type).toBe('turn_finished');
    if (finished?.type === 'turn_finished') expect(finished.status).toBe('succeeded');
  });

  it('provider_unavailable_does_not_switch_route', async () => {
    const requestedRoute = { connection_id: 'local', model_id: 'requested-model-abc', harness: 'opencode' as const };
    // The native session offers only a different model: a fallback the adapter
    // must never adopt in place of the requested route.
    const double = makeDouble({ models: ['fallback-model-xyz'] });
    const adapter = adapterFor(double);
    const sessionInput = input('opencode', { route: requestedRoute });
    const ref = await openSession(adapter, sessionInput);

    expect(canonical(ref.resolved_route)).toBe(canonical(requestedRoute));
    expect(ref.resolved_route.model_id).toBe('requested-model-abc');

    const events = await runTurn(adapter, ref, sessionInput);
    expect(events.length).toBeGreaterThan(0);
    expect(JSON.stringify(events)).not.toContain('fallback-model-xyz');

    // An unavailable connection refuses outright instead of opening on any route.
    const unavailable = new OpenCodeAdapter(denyingHost, { binary: '/nonexistent/opencode', stateDir: join(double.dir, 'unavailable') });
    await expect(unavailable.open(input('opencode', { route: requestedRoute }))).rejects.toThrow(/opencodeUnavailable/);
  });

  it('resume_cannot_cross_project', async () => {
    const double = makeDouble();
    const adapter = adapterFor(double);
    const foreign: SessionRef = {
      harness: 'opencode',
      native_id: 'other-project-native-session',
      native_version: '0.19.0',
      project_id: 'another-project',
      cwd: '/elsewhere',
      resolved_route: { connection_id: 'local', model_id: 'm', harness: 'opencode' },
    };
    await expect(adapter.open(input('opencode', { resume: foreign }))).rejects.toThrow('SessionIdentityMismatch');

    // Refused before any native session process existed: only the version probe ran.
    const log = readDoubleLog(double);
    expect(log).toContain('INVOKE --version');
    expect(log).not.toContain('INVOKE acp');
    expect(log).not.toContain('SESSION_RESUME');

    // Positive control: a same-project resume is honoured, so the guard above is
    // a real identity check rather than an unconditional failure.
    const own: SessionRef = {
      harness: 'opencode',
      native_id: 'own-native-session',
      native_version: '0.19.0',
      project_id: 'proj-1',
      cwd: '/tmp',
      resolved_route: { connection_id: 'local', model_id: 'm', harness: 'opencode' },
    };
    const ownDouble = makeDouble();
    const resumed = await openSession(adapterFor(ownDouble), input('opencode', { resume: own }));
    expect(resumed.project_id).toBe('proj-1');
    expect(resumed.native_id).toBe('own-native-session');
    expect(readDoubleLog(ownDouble)).toContain('SESSION_RESUME own-native-session');
  });

  it('unexpected_listener_is_unqualified', async () => {
    // The probed binary advertises a loopback listener beyond the agreed stdio
    // transport; describe() must never accept it as a qualified capability.
    const double = makeDouble({ version: 'opencode 0.19.0 (loopback listener 127.0.0.1:4096)' });
    const adapter = adapterFor(double);
    const descriptor = await adapter.describe();

    expect(descriptor.transport).toBe('stdio-acp');
    expect(descriptor.version).toBe('0.19.0');
    expect(descriptor.capabilities.length).toBeGreaterThan(0);
    for (const capability of descriptor.capabilities) expect(capability.state).toBe('unqualified');
    expect(descriptor.capabilities.some(capability => capability.state === 'qualified')).toBe(false);
    const listenerCapability = descriptor.capabilities.find(capability => /listen|port|network|server/i.test(capability.name));
    if (listenerCapability) expect(listenerCapability.state).toBe('unqualified');

    // Real observed argv: the session spawn carries no listener/port transport.
    await openSession(adapter, input('opencode'));
    const invocations = readDoubleLog(double).split('\n').filter(line => line.startsWith('INVOKE'));
    expect(invocations.some(line => line.includes('acp'))).toBe(true);
    for (const invocation of invocations) expect(invocation).not.toMatch(/--port|--host|--listen|--serve|--cwd/i);
  });
});
