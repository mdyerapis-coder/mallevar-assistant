import { describe, it, expect } from 'bun:test';
import { chmodSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { HermesAdapter } from '../src/harness/hermes';
import type { NativePermissionHost, NativePermissionRequest } from '../src/harness/native/host';
import type {
  OpenSessionInput,
  ProjectPolicy,
  SessionRef,
  TurnInput,
  WorkerEvent,
} from '../../contracts/src/generated';

/** A real executable double for `hermes`. It answers the ACP handshake, records every argv it was
 *  spawned with, records its own pid and the replies it receives, and writes an `allowed.sentinel`
 *  only if it is told a permission was granted — so a bypass would be observable. It needs no LLM
 *  provider: it is a deterministic stdio JSON-RPC peer. */
const DOUBLE_SOURCE = [
  "import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';",
  "import { join } from 'node:path';",
  '',
  'const argv = process.argv.slice(2);',
  'const home = process.env.HERMES_HOME || process.cwd();',
  'mkdirSync(home, { recursive: true });',
  'function ev(name) { return join(home, name); }',
  "appendFileSync(ev('argv.jsonl'), JSON.stringify(argv) + '\\n');",
  '',
  "if (argv.indexOf('--version') !== -1) {",
  "  process.stdout.write('Hermes Agent v1.2.3\\n');",
  '  process.exit(0);',
  '}',
  "if (argv[0] !== 'acp') {",
  "  process.stderr.write('hermes-double: unexpected argv ' + JSON.stringify(argv) + '\\n');",
  '  process.exit(2);',
  '}',
  '',
  "writeFileSync(ev('pid'), String(process.pid));",
  '',
  "let mode = 'end';",
  "try { mode = readFileSync(ev('mode'), 'utf8').trim() || 'end'; } catch { mode = 'end'; }",
  '',
  'const PERMISSION_ID = 900;',
  'let buf = "";',
  'let pendingPrompt = null;',
  '',
  'function send(obj) { process.stdout.write(JSON.stringify(obj) + "\\n"); }',
  '',
  'function onServerRequestReply(msg) {',
  "  appendFileSync(ev('replies.jsonl'), JSON.stringify(msg) + '\\n');",
  '  const result = msg.result;',
  '  const outcome = result && typeof result === "object" ? result.outcome : undefined;',
  '  const optionId = outcome && typeof outcome === "object" ? outcome.optionId : undefined;',
  '  const allowed = !!outcome && outcome.outcome === "selected" && typeof optionId === "string" && optionId.indexOf("allow") === 0;',
  "  if (allowed) writeFileSync(ev('allowed.sentinel'), 'allowed');",
  '  if (pendingPrompt !== null) {',
  '    const id = pendingPrompt;',
  '    pendingPrompt = null;',
  '    send({ jsonrpc: "2.0", id: id, result: { stopReason: "end_turn" } });',
  '  }',
  '}',
  '',
  'function handle(msg) {',
  '  if (!msg || typeof msg !== "object") return;',
  '  if (typeof msg.id === "number" && typeof msg.method !== "string") { onServerRequestReply(msg); return; }',
  '  if (msg.method === "session/cancel") {',
  '    if (pendingPrompt !== null) {',
  '      const id = pendingPrompt;',
  '      pendingPrompt = null;',
  '      send({ jsonrpc: "2.0", id: id, result: { stopReason: "cancelled" } });',
  '    }',
  '    return;',
  '  }',
  '  if (typeof msg.method !== "string" || typeof msg.id !== "number") return;',
  '  const id = msg.id;',
  '  const params = msg.params;',
  '  if (msg.method === "initialize") {',
  '    send({ jsonrpc: "2.0", id: id, result: { protocolVersion: 1, agentInfo: { name: "hermes-double", version: "1.2.3" } } });',
  '  } else if (msg.method === "session/new") {',
  '    send({ jsonrpc: "2.0", id: id, result: { sessionId: "native-sess-1", models: { availableModels: [{ modelId: "m" }] } } });',
  '  } else if (msg.method === "session/resume") {',
  '    const sid = params && typeof params.sessionId === "string" ? params.sessionId : "native-sess-1";',
  '    send({ jsonrpc: "2.0", id: id, result: { sessionId: sid, models: { availableModels: [{ modelId: "m" }] } } });',
  '  } else if (msg.method === "session/load") {',
  '    send({ jsonrpc: "2.0", id: id, result: {} });',
  '  } else if (msg.method === "session/prompt") {',
  '    if (mode === "idle") { pendingPrompt = id; return; }',
  '    if (mode === "permission") {',
  '      pendingPrompt = id;',
  '      send({ jsonrpc: "2.0", id: PERMISSION_ID, method: "session/request_permission", params: { toolCall: { toolCallId: "tc-1", name: "bash", title: "run shell", kind: "execute", rawInput: { command: "echo hi" } }, options: [{ optionId: "allow-once", kind: "allow_once" }, { optionId: "deny", kind: "reject_once" }] } });',
  '      return;',
  '    }',
  '    if (mode === "hook") {',
  '      pendingPrompt = id;',
  '      send({ jsonrpc: "2.0", id: PERMISSION_ID, method: "hook/request", params: { hook: "post-tool", activation: null } });',
  '      return;',
  '    }',
  '    send({ jsonrpc: "2.0", id: id, result: { stopReason: "end_turn" } });',
  '  } else {',
  '    send({ jsonrpc: "2.0", id: id, result: {} });',
  '  }',
  '}',
  '',
  'process.stdin.setEncoding("utf8");',
  'process.stdin.on("data", (chunk) => {',
  '  buf += chunk;',
  '  let idx;',
  '  while ((idx = buf.indexOf("\\n")) !== -1) {',
  '    const line = buf.slice(0, idx);',
  '    buf = buf.slice(idx + 1);',
  '    if (line.trim().length === 0) continue;',
  '    let msg;',
  '    try { msg = JSON.parse(line); } catch { continue; }',
  '    handle(msg);',
  '  }',
  '});',
  'process.stdin.on("end", () => process.exit(0));',
  '',
].join('\n');

/** Flags that would make the harness start in a one-shot / auto-approve mode. */
const BYPASS_FLAGS = [
  '-z',
  '--oneshot',
  '-oneshot',
  '--accept-hooks',
  '--auto-approve',
  '--auto_approve',
  '--yolo',
  '--dangerously-skip-permissions',
  '--bypass-permissions',
  '--no-approval',
  '--non-interactive',
];

const PROJECT = 'proj-hermes';
const PERMISSION_REQUEST_ID = 900;

type Evidence = {
  argv: string[][];
  replies: Array<Record<string, unknown>>;
  pid?: number;
};

function policy(): ProjectPolicy {
  return {
    version: "1",
    project_id: PROJECT,
    allowed_connection_ids: ['local'],
    allowed_tools: ['read'],
    read_roots: [],
    write_roots: [],
    network_destinations: [],
    local_only: true,
    max_workers: 1,
    max_requests: 4,
    max_cost_microusd: "0",
    spend_policy: 'free_only',
    allow_unknown_cost: false,
  };
}

function sessionInput(cwd: string, resume?: SessionRef): OpenSessionInput {
  return {
    run_id: 'run-hermes',
    project_id: PROJECT,
    cwd,
    route: { connection_id: 'local', model_id: 'm', harness: 'hermes' },
    context: {
      digest: 'd',
      brief: '',
      constraints: [],
      excerpts: [],
      artifacts: [],
      unresolved_questions: [],
      originating_sessions: [],
    },
    policy: policy(),
    resume,
  };
}

function turnInput(budget: ProjectPolicy): TurnInput {
  return {
    request_id: 'req-hermes',
    message_id: 'msg-hermes',
    content: [{ type: 'text', text: 'hello' }],
    allowed_tools: [{ name: 'read', description: '', input_schema: {} }],
    budget,
  };
}

/** Write the double plus an executable wrapper (absolute bun path, so PATH is irrelevant) into a
 *  fresh state dir. The wrapper's pid is the same as the child's because it `exec`s bun. */
function makeState(mode: string): { dir: string; binary: string } {
  const dir = join(
    tmpdir(),
    `ironbark-hermes-perms-${mode}-${process.pid}-${Date.now()}-${Math.floor(Math.random() * 1e9)}`,
  );
  mkdirSync(dir, { recursive: true });
  const script = join(dir, 'hermes-double.mjs');
  const binary = join(dir, 'hermes-double');
  writeFileSync(script, DOUBLE_SOURCE);
  writeFileSync(
    binary,
    '#!/bin/sh\nexec ' + JSON.stringify(process.execPath) + ' ' + JSON.stringify(script) + ' "$@"\n',
  );
  chmodSync(binary, 0o755);
  writeFileSync(join(dir, 'mode'), mode);
  return { dir, binary };
}

function jsonLines<T>(path: string): Array<T> {
  if (!existsSync(path)) return [];
  return readFileSync(path, 'utf8')
    .split('\n')
    .filter(line => line.trim().length > 0)
    .map(line => JSON.parse(line) as T);
}

function readEvidence(dir: string): Evidence {
  const pidPath = join(dir, 'pid');
  const pid = existsSync(pidPath) ? Number(readFileSync(pidPath, 'utf8').trim()) : undefined;
  return {
    argv: jsonLines<string[]>(join(dir, 'argv.jsonl')),
    replies: jsonLines<Record<string, unknown>>(join(dir, 'replies.jsonl')),
    pid,
  };
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** Host that consults the core for every native permission request and never silently allows. */
function makeHost(): {
  host: NativePermissionHost;
  calls: NativePermissionRequest[];
  events: Array<Record<string, unknown>>;
} {
  const calls: NativePermissionRequest[] = [];
  const events: Array<Record<string, unknown>> = [];
  const host: NativePermissionHost = {
    async state() {
      return { policy_version: "1", state_digest: 'd'.repeat(64) };
    },
    async prepareAction(request) {
      calls.push(request);
      // Core decision: no grant exists for this action, so the request fails closed.
      throw new Error('CoreDeniedUnknownPermission');
    },
    async verifyAnswer() {
      return true;
    },
    async recordNativeEvent(event) {
      events.push(event);
    },
  };
  return { host, calls, events };
}

describe('hermes adapter permissions', () => {
  it('headless_mode_cannot_bypass_approval', async () => {
    const { dir, binary } = makeState('permission');
    const { host, calls, events } = makeHost();
    const adapter = new HermesAdapter(host, { binary, stateDir: dir });
    const input = sessionInput(dir);
    let ref: SessionRef | undefined;
    try {
      ref = await adapter.open(input);
      const collected: WorkerEvent[] = [];
      for await (const event of adapter.turn(ref, turnInput(input.policy))) collected.push(event);

      const evidence = readEvidence(dir);

      // 1. The adapter must never launch Hermes in a one-shot / auto-approve mode.
      expect(evidence.argv).toContainEqual(['acp']); // exactly the plain ACP entry point
      for (const argv of evidence.argv) {
        for (const flag of BYPASS_FLAGS) {
          expect(argv).not.toContain(flag);
        }
      }

      // 2. The permission path still consults the host (the core), not the model or the adapter.
      expect(calls.length).toBe(1);
      expect(calls[0].name).toBe('acp.toolPermission');
      expect(calls[0].tool_call_id).toBe('tc-1');
      const requestInput = calls[0].input;
      if (typeof requestInput !== 'object' || requestInput === null || !('name' in requestInput)) {
        throw new Error('permission request carried no tool input');
      }
      expect(requestInput.name).toBe('bash');
      expect(events.some(event => event['event'] === 'approval_failed_closed')).toBe(true);

      // 3. The denial is propagated: the harness is told to reject, never to allow.
      const permissionReply = evidence.replies.find(frame => frame['id'] === PERMISSION_REQUEST_ID);
      expect(permissionReply).toBeDefined();
      const permissionResult = permissionReply?.['result'];
      const outcome =
        typeof permissionResult === 'object' && permissionResult !== null && 'outcome' in permissionResult
          ? permissionResult.outcome
          : undefined;
      const optionId =
        typeof outcome === 'object' && outcome !== null && 'optionId' in outcome ? outcome.optionId : undefined;
      expect(optionId).toBe('deny');
      const deniedFileExists = existsSync(join(dir, 'allowed.sentinel'));
      expect(deniedFileExists).toBe(false);

      const finished = collected.find(event => event.payload.type === 'turn_finished');
      if (!finished || finished.payload.type !== 'turn_finished') throw new Error('turn did not finish');
      expect(finished.payload.status).toBe('succeeded');
    } finally {
      if (ref) await adapter.close(ref).catch(() => undefined);
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('profile_model_mismatch_blocks_resume', async () => {
    const { dir, binary } = makeState('end');
    const { host } = makeHost();
    const adapter = new HermesAdapter(host, { binary, stateDir: dir });
    const input = sessionInput(dir);
    let ref: SessionRef | undefined;
    try {
      // A resume carrying a different model than the current route is refused.
      const modelMismatch: SessionRef = {
        harness: 'hermes',
        native_id: 'native-sess-1',
        native_version: '1.2.3',
        project_id: PROJECT,
        cwd: dir,
        resolved_route: { connection_id: 'local', model_id: 'different-model', harness: 'hermes' },
      };
      await expect(adapter.open({ ...input, resume: modelMismatch })).rejects.toThrow('SessionIdentityMismatch');

      // A resume carrying a different profile (project) is refused.
      const profileMismatch: SessionRef = { ...modelMismatch, project_id: 'another-project' };
      await expect(adapter.open({ ...input, resume: profileMismatch })).rejects.toThrow('SessionIdentityMismatch');

      // Both refusals happened before the adapter ever dispatched `hermes acp`.
      const mismatchedResumeDispatchCount = readEvidence(dir).argv.filter(argv => argv[0] === 'acp').length;
      expect(mismatchedResumeDispatchCount).toBe(0);

      // Control: the identical identity is not refused — the check is about the mismatch only.
      const matched: SessionRef = { ...modelMismatch, resolved_route: { ...input.route } };
      ref = await adapter.open({ ...input, resume: matched });
      expect(ref.native_id).toBe('native-sess-1');
    } finally {
      if (ref) await adapter.close(ref).catch(() => undefined);
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('hook_request_without_activation_is_denied', async () => {
    const { dir, binary } = makeState('hook');
    const { host, calls, events } = makeHost();
    const adapter = new HermesAdapter(host, { binary, stateDir: dir });
    const input = sessionInput(dir);
    let ref: SessionRef | undefined;
    try {
      ref = await adapter.open(input);
      const collected: WorkerEvent[] = [];
      for await (const event of adapter.turn(ref, turnInput(input.policy))) collected.push(event);

      const evidence = readEvidence(dir);
      const hookReply = evidence.replies.find(frame => frame['id'] === PERMISSION_REQUEST_ID);
      expect(hookReply).toBeDefined();
      const hookResult = hookReply?.['result'];
      const denied =
        typeof hookResult === 'object' && hookResult !== null && 'denied' in hookResult ? hookResult.denied : undefined;
      // A hook/extension request with no activation fails closed: a hard deny, not an approval.
      expect(denied).toBe(true);
      // It never entered the permission path, so the core was never asked to grant anything.
      expect(calls.length).toBe(0);
      const denial = events.find(event => event['event'] === 'server_request_denied');
      expect(denial).toBeDefined();
      expect(denial?.['method']).toBe('hook/request');
      const deniedFileExists = existsSync(join(dir, 'allowed.sentinel'));
      expect(deniedFileExists).toBe(false);
      const finished = collected.find(event => event.payload.type === 'turn_finished');
      if (!finished || finished.payload.type !== 'turn_finished') throw new Error('turn did not finish');
      expect(finished.payload.status).toBe('succeeded');
    } finally {
      if (ref) await adapter.close(ref).catch(() => undefined);
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it('cancel_waits_for_native_turn_stop_and_close_observes_owned_process_exit', async () => {
    const { dir, binary } = makeState('idle');
    // Release admission, then synchronize on the actual streamed turn start.
    const gate = Promise.withResolvers<void>();
    const events: Array<Record<string, unknown>> = [];
    const host: NativePermissionHost = {
      async state() {
        await gate.promise;
        return { policy_version: "1", state_digest: 'd'.repeat(64) };
      },
      async prepareAction() {
        throw new Error('unexpected permission request during cancel test');
      },
      async verifyAnswer() {
        return true;
      },
      async recordNativeEvent(event) {
        events.push(event);
      },
    };
    const adapter = new HermesAdapter(host, { binary, stateDir: dir });
    const input = sessionInput(dir);
    let ref: SessionRef | undefined;
    let closed = false;
    try {
      const session: SessionRef = await adapter.open(input);
      ref = session;
      const collected: WorkerEvent[] = [];
      let consumeError: unknown;
      const turnStarted = Promise.withResolvers<void>();
      const consume = (async () => {
        for await (const event of adapter.turn(session, turnInput(input.policy))) {
          collected.push(event);
          if (event.payload.type === 'turn_started') turnStarted.resolve();
        }
      })().catch(error => {
        consumeError = error;
      });

      gate.resolve();
      await turnStarted.promise;
      await adapter.interrupt(session);

      await consume;
      expect(consumeError).toBeUndefined();
      const finished = collected.find(event => event.payload.type === 'turn_finished');
      if (!finished || finished.payload.type !== 'turn_finished') throw new Error('turn did not finish');
      expect(finished.payload.status).toBe('cancelled');

      // A cancel is a request, not an observed stop: the child is still alive and nothing has
      // been reported as terminated.
      const afterCancel = readEvidence(dir);
      const pid = afterCancel.pid;
      if (pid === undefined) throw new Error('double never recorded its pid');
      expect(isAlive(pid)).toBe(true);
      expect(events.some(event => event['event'] === 'session_closed')).toBe(false);

      // Only close() observes the real child exit before reporting the stop.
      await adapter.close(session);
      closed = true;
      ref = undefined;
      const sessionClosed = events.find(event => event['event'] === 'session_closed');
      expect(sessionClosed).toBeDefined();
      const exit = sessionClosed?.['exit'];
      expect(typeof exit === 'string' && /^(exit=\d+|signal=)/.test(exit)).toBe(true);
      expect(isAlive(pid)).toBe(false);
      const stopReceipt = { all_terminated: !isAlive(pid) };
      expect(stopReceipt.all_terminated).toBe(true);
    } finally {
      if (ref && !closed) await adapter.close(ref).catch(() => undefined);
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
