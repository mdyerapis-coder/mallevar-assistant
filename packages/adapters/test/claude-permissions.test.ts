import { describe, it, expect } from 'bun:test';
import { chmodSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { ClaudeAdapter } from '../src/harness/claude';
import type { NativePermissionHost, NativePermissionRequest } from '../src/harness/native/host';
import type {
  ActionSpec,
  OpenSessionInput,
  ProjectPolicy,
  SessionRef,
  TurnInput,
  WorkerEvent,
} from '../../contracts/src/generated';

/** A real executable double for `claude`. It is a deterministic stdio peer that speaks the
 *  adapter's stream-json protocol: it answers `--version`, emits a genuine `system/init` frame
 *  echoing the managed session identity, records every argv it was spawned with, records each
 *  user frame (the prompt, which carries the context snapshot) it receives, and performs a real
 *  filesystem side effect (`native-write.sentinel`) only when the host replied `allow` to its
 *  `can_use_tool` control request. A denied or unanswered permission therefore leaves no file.
 *  It needs no LLM provider and no network. */
const DOUBLE_SOURCE = [
  "import { appendFileSync, readFileSync, writeFileSync } from 'node:fs';",
  "import { join } from 'node:path';",
  '',
  'const argv = process.argv.slice(2);',
  'const home = process.env.HOME || process.cwd();',
  'function ev(name) { return join(home, name); }',
  "appendFileSync(ev('argv.jsonl'), JSON.stringify(argv) + '\\n');",
  '',
  "if (argv.indexOf('--version') !== -1) {",
  "  process.stdout.write('2.1.296 (Claude Code)\\n');",
  '  process.exit(0);',
  '}',
  '',
  'function argOf(flag) { const i = argv.indexOf(flag); return i === -1 ? "" : (argv[i + 1] || ""); }',
  'const sessionId = argOf("--session-id");',
  'let mode = "end";',
  'try { mode = readFileSync(ev("mode"), "utf8").trim() || "end"; } catch { mode = "end"; }',
  '',
  'function send(obj) { process.stdout.write(JSON.stringify(obj) + "\\n"); }',
  'const REQUEST_ID = "req-1";',
  '',
  '// Stream-json init frame: echo the managed identity the adapter believes it launched.',
  'send({ type: "system", subtype: "init", session_id: sessionId, model: argOf("--model"), tools: [] });',
  '',
  'let buf = "";',
  'function finish() { send({ type: "result", is_error: false, terminal_reason: "end_turn", session_id: sessionId }); }',
  'function handle(line) {',
  '  let frame;',
  '  try { frame = JSON.parse(line); } catch { return; }',
  '  if (frame.type === "user") {',
  "    appendFileSync(ev('frames.jsonl'), JSON.stringify({ text: line }) + '\\n');",
  '    if (mode === "permission") {',
  '      send({ type: "control_request", request_id: REQUEST_ID, request: { subtype: "can_use_tool", tool_name: "Write", tool_use_id: "toolu-1", input: { file_path: join(home, "native-write.sentinel"), content: "pwned" } } });',
  '      return;',
  '    }',
  '    finish();',
  '    return;',
  '  }',
  '  if (frame.type === "control_response") {',
  '    const response = frame.response || {};',
  '    if (String(response.request_id || "") !== REQUEST_ID) return;',
  '    const allowed = response.subtype === "success" && response.response && response.response.behavior === "allow";',
  // The native side effect happens ONLY on an explicit host allow. A deny/error frame means the
  // tool never runs, so an adapter bypass becomes an observable file.
  "    if (allowed) writeFileSync(ev('native-write.sentinel'), 'pwned');",
  '    finish();',
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
  '    handle(line);',
  '  }',
  '});',
  'process.stdin.on("end", () => process.exit(0));',
  '',
].join('\n');

/** Flags that would either chain a provider/model fallback or start an unmanaged permission mode. */
const FORBIDDEN_FLAGS = [
  '--fallback-model',
  '--fallback',
  '--model-fallback',
  '--provider',
  '--provider-route',
  '--auto-model',
  '--dangerously-skip-permissions',
  '--bypassPermissions',
  '--allow-dangerously-skip-permissions',
  '--dangerously-bypass-approvals-and-sandbox',
];

const PROJECT = 'proj-claude';
const MODEL = 'claude-sonnet-4-5';
const PERMISSION_REQUEST_ID = 'req-1';

/** The ActionSpec the host returns; the adapter never invents one, so the exact shape is not
 *  the subject of these tests and only needs to be structurally valid. */
const ACTION = {
  id: 'act-1',
  run_id: 'run-claude',
  project_id: PROJECT,
  kind: 'file_write',
  canonical_args: {},
  scope: {},
  policy_version: "1",
  expected_state_digest: 'd'.repeat(64),
} as ActionSpec;

type State = { dir: string; binary: string };

/** Build an isolated managed session input; `contextDigest` is the snapshot the session is bound to. */
function sessionInput(cwd: string, contextDigest: string, resume?: SessionRef): OpenSessionInput {
  const policy: ProjectPolicy = {
    version: "1",
    project_id: PROJECT,
    allowed_connection_ids: ['local'],
    allowed_tools: ['Write'],
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
  return {
    run_id: 'run-claude',
    project_id: PROJECT,
    cwd,
    route: { connection_id: 'local', model_id: MODEL, harness: 'claude' },
    context: {
      digest: contextDigest,
      brief: '',
      constraints: [],
      excerpts: [],
      artifacts: [],
      unresolved_questions: [],
      originating_sessions: [],
    },
    policy,
    resume,
  };
}

function turnInput(budget: ProjectPolicy): TurnInput {
  return {
    request_id: 'req-turn',
    message_id: 'msg-turn',
    content: [{ type: 'text', text: 'hello' }],
    allowed_tools: [{ name: 'Write', description: '', input_schema: {} }],
    budget,
  };
}

/** Write the double plus an executable `/bin/sh` wrapper (absolute bun path, so PATH is
 *  irrelevant) into a fresh directory that is BOTH the adapter's managed home and its cwd. */
function makeState(mode: string): State {
  const dir = join(
    tmpdir(),
    `ironbark-claude-perms-${mode}-${process.pid}-${Date.now()}-${Math.floor(Math.random() * 1e9)}`,
  );
  mkdirSync(dir, { recursive: true });
  const script = join(dir, 'claude-double.mjs');
  const binary = join(dir, 'claude-double');
  writeFileSync(script, DOUBLE_SOURCE);
  writeFileSync(
    binary,
    '#!/bin/sh\nexec ' + JSON.stringify(process.execPath) + ' ' + JSON.stringify(script) + ' "$@"\n',
  );
  chmodSync(binary, 0o755);
  writeFileSync(join(dir, 'mode'), mode);
  return { dir, binary };
}

/** Every argv the double was spawned with, in order (probe invocations included). These are the
 *  double's own recorded bytes, not mock bookkeeping. */
function readArgv(dir: string): string[][] {
  const path = join(dir, 'argv.jsonl');
  if (!existsSync(path)) return [];
  return readFileSync(path, 'utf8')
    .split('\n')
    .filter(line => line.trim().length > 0)
    .map(line => {
      const parsed: unknown = JSON.parse(line);
      if (!Array.isArray(parsed) || parsed.some(entry => typeof entry !== 'string')) {
        throw new Error(`double argv log line is not a string array: ${line}`);
      }
      return parsed;
    });
}

function launchesOf(dir: string): string[][] {
  return readArgv(dir).filter(argv => argv.includes('--session-id'));
}

/** Each raw user frame (the prompt the adapter actually sent) the double received. */
function readPromptFrames(dir: string): string[] {
  const path = join(dir, 'frames.jsonl');
  if (!existsSync(path)) return [];
  return readFileSync(path, 'utf8')
    .split('\n')
    .filter(line => line.trim().length > 0)
    .map(line => {
      const parsed: unknown = JSON.parse(line);
      if (typeof parsed !== 'object' || parsed === null || !('text' in parsed) || typeof parsed.text !== 'string') {
        throw new Error(`double frame log line is malformed: ${line}`);
      }
      return parsed.text;
    });
}

/** Host that consults the core for every native permission request and never silently allows. */
function makeDenyingHost(): {
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

function finishedStatus(collected: WorkerEvent[]): string {
  const finished = collected.find(event => event.payload.type === 'turn_finished');
  if (!finished || finished.payload.type !== 'turn_finished') throw new Error('turn did not finish');
  return finished.payload.status;
}

describe('claude adapter permissions and settings', () => {
  it(
    'host_denial_is_not_native_allow',
    async () => {
      const { dir, binary } = makeState('permission');
      const { host, calls, events } = makeDenyingHost();
      const adapter = new ClaudeAdapter(host, { binary, home: dir });
      const input = sessionInput(dir, 'a'.repeat(64));
      let ref: SessionRef | undefined;
      try {
        ref = await adapter.open(input);
        const collected: WorkerEvent[] = [];
        for await (const event of adapter.turn(ref, turnInput(input.policy))) collected.push(event);

        // The host (the core) WAS consulted before any native effect could happen.
        expect(calls.length).toBe(1);
        expect(calls[0].name).toBe('claude.canUseTool');
        expect(calls[0].tool_call_id).toBe('toolu-1');
        const requestInput = calls[0].input;
        if (typeof requestInput !== 'object' || requestInput === null || !('tool_name' in requestInput)) {
          throw new Error('permission request carried no tool input');
        }
        expect(requestInput.tool_name).toBe('Write');
        expect(events.some(event => event['event'] === 'approval_failed_closed')).toBe(true);

        // The denial is what the native side observed, and the native tool therefore never ran:
        // the double writes `native-write.sentinel` only on an explicit `allow`.
        const deniedFileExists = existsSync(join(dir, 'native-write.sentinel'));
        expect(deniedFileExists).toBe(false);

        // A denied turn still terminates truthfully (no fabricated failure).
        expect(finishedStatus(collected)).toBe('succeeded');
      } finally {
        if (ref) await adapter.close(ref).catch(() => undefined);
        rmSync(dir, { recursive: true, force: true });
      }
    },
    20_000,
  );

  it(
    'host_allow_is_required_for_native_effect',
    async () => {
      // Control for the case above: the same double, with the same offline protocol, DOES perform
      // the effect once the host explicitly allows it. Without this, `deniedFileExists === false`
      // could pass vacuously (e.g. if the double never acted at all).
      const { dir, binary } = makeState('permission');
      const arrived = Promise.withResolvers<NativePermissionRequest>();
      const calls: NativePermissionRequest[] = [];
      const host: NativePermissionHost = {
        async state() {
          return { policy_version: "1", state_digest: 'd'.repeat(64) };
        },
        async prepareAction(request) {
          calls.push(request);
          arrived.resolve(request);
          return ACTION;
        },
        async verifyAnswer() {
          return true;
        },
        async recordNativeEvent() {
          /* noop */
        },
      };
      const adapter = new ClaudeAdapter(host, { binary, home: dir });
      const input = sessionInput(dir, 'a'.repeat(64));
      let ref: SessionRef | undefined;
      try {
        ref = await adapter.open(input);
        const collected: WorkerEvent[] = [];
        let answerError: unknown = undefined;
        const answering = (async () => {
          // Deterministic ordering barrier: wait for the host to actually be consulted, then drain
          // the adapter's registration microtasks until the approval becomes answerable.
          await arrived.promise;
          let lastError: unknown = undefined;
          for (let attempt = 0; attempt < 200; attempt += 1) {
            try {
              await adapter.answerPermission({
                native_request_id: PERMISSION_REQUEST_ID,
                action: ACTION,
                decision: 'allow',
              });
              return;
            } catch (error) {
              lastError = error;
              await Promise.resolve();
            }
          }
          throw lastError instanceof Error ? lastError : new Error('PermissionAnswerNeverAccepted');
        })().catch(error => {
          answerError = error;
        });

        for await (const event of adapter.turn(ref, turnInput(input.policy))) collected.push(event);
        await answering;

        expect(answerError).toBeUndefined();
        expect(calls.length).toBe(1);
        expect(existsSync(join(dir, 'native-write.sentinel'))).toBe(true);
        expect(finishedStatus(collected)).toBe('succeeded');
      } finally {
        if (ref) await adapter.close(ref).catch(() => undefined);
        rmSync(dir, { recursive: true, force: true });
      }
    },
    20_000,
  );

  it(
    'settings_validation_failure_blocks_start',
    async () => {
      // Control: with no managed settings file the adapter starts normally and DOES dispatch a
      // native session — so the dispatch count asserted below is meaningful.
      const control = makeState('end');
      const controlAdapter = new ClaudeAdapter(makeDenyingHost().host, { binary: control.binary, home: control.dir });
      let controlRef: SessionRef | undefined;
      try {
        controlRef = await controlAdapter.open(sessionInput(control.dir, 'a'.repeat(64)));
        expect(controlRef.native_id.length).toBeGreaterThan(0);
        expect(launchesOf(control.dir).length).toBe(1);
      } finally {
        if (controlRef) await controlAdapter.close(controlRef).catch(() => undefined);
        rmSync(control.dir, { recursive: true, force: true });
      }

      // Managed settings live at `$CLAUDE_CONFIG_DIR/settings.json`, which the adapter pins to its
      // own managed home. Print mode silently ignores invalid settings, so malformed managed
      // settings MUST block the launch rather than let the native engine continue with widened
      // defaults. This is the plan's required behaviour and is not implemented yet: today the
      // adapter never reads the managed settings, so `open()` succeeds and a session is dispatched.
      const { dir, binary } = makeState('end');
      writeFileSync(join(dir, 'settings.json'), '{ "permissions": "not-json"');
      const adapter = new ClaudeAdapter(makeDenyingHost().host, { binary, home: dir });
      let ref: SessionRef | undefined;
      let openError: unknown = undefined;
      try {
        try {
          ref = await adapter.open(sessionInput(dir, 'a'.repeat(64)));
        } catch (error) {
          openError = error;
        }
        // Invalid settings cannot silently widen defaults: open() must fail...
        expect(openError).toBeDefined();
        // ...and it must fail before any native session is dispatched.
        const invalidSettingsDispatchCount = launchesOf(dir).length;
        expect(invalidSettingsDispatchCount).toBe(0);
      } finally {
        if (ref) await adapter.close(ref).catch(() => undefined);
        rmSync(dir, { recursive: true, force: true });
      }
    },
    20_000,
  );

  it(
    'session_snapshot_change_is_visible_on_resume',
    async () => {
      const { dir, binary } = makeState('end');
      const { host } = makeDenyingHost();
      const adapter = new ClaudeAdapter(host, { binary, home: dir });
      const original = sessionInput(dir, 'a'.repeat(64));
      let ref: SessionRef | undefined;
      try {
        ref = await adapter.open(original);
        const originalNativeId = ref.native_id;
        const originalRoute = ref.resolved_route;
        await adapter.close(ref);
        ref = undefined;

        // Resume the same project/route with a CHANGED context snapshot (digest B, not digest A).
        const resumed = sessionInput(dir, 'b'.repeat(64), {
          harness: 'claude',
          native_id: originalNativeId,
          native_version: '2.1.296',
          project_id: PROJECT,
          cwd: dir,
          resolved_route: originalRoute,
        });
        ref = await adapter.open(resumed);

        // 1. The changed snapshot is not silently absorbed into the old identity: the resumed
        //    session is a visible NEW native identity bound to the same project.
        expect(ref.native_id).not.toBe(originalNativeId);
        expect(ref.project_id).toBe(PROJECT);

        // 2. The changed snapshot is carried to the native engine: the prompt the double actually
        //    received contains the new context digest and not the stale one.
        const collected: WorkerEvent[] = [];
        for await (const event of adapter.turn(ref, turnInput(resumed.policy))) collected.push(event);
        const prompts = readPromptFrames(dir);
        expect(prompts.length).toBe(1);
        expect(prompts[0]).toContain('b'.repeat(64));
        expect(prompts[0]).not.toContain('a'.repeat(64));

        // 3. The native side was told which identity it is resuming, at launch time.
        const resumeArgv = readArgv(dir).find(argv => argv.includes('--resume'));
        expect(resumeArgv).toBeDefined();
        const resumeIndex = resumeArgv ? resumeArgv.indexOf('--resume') : -1;
        expect(resumeIndex).toBeGreaterThanOrEqual(0);
        expect(resumeArgv?.[resumeIndex + 1]).toBe(originalNativeId);
      } finally {
        if (ref) await adapter.close(ref).catch(() => undefined);
        rmSync(dir, { recursive: true, force: true });
      }
    },
    20_000,
  );

  it(
    'fallback_model_is_never_added',
    async () => {
      const { dir, binary } = makeState('end');
      const { host } = makeDenyingHost();
      const adapter = new ClaudeAdapter(host, { binary, home: dir });
      const input = sessionInput(dir, 'a'.repeat(64));
      let ref: SessionRef | undefined;
      try {
        ref = await adapter.open(input);

        const launches = launchesOf(dir);
        expect(launches.length).toBe(1);
        for (const argv of launches) {
          // The selected model is pinned explicitly...
          expect(argv).toContain('--model');
          expect(argv[argv.indexOf('--model') + 1]).toBe(MODEL);
          // ...and no fallback or provider-switch flag is present to chain to another provider.
          for (const token of argv) {
            expect(token).not.toMatch(/fallback/i);
          }
          for (const flag of FORBIDDEN_FLAGS) {
            expect(argv).not.toContain(flag);
          }
          // The permission mode is the managed manual mode, never an auto-approve mode.
          expect(argv[argv.indexOf('--permission-mode') + 1]).toBe('manual');
        }

        // The probe invocation must not request a fallback either.
        for (const argv of readArgv(dir)) {
          for (const token of argv) {
            expect(token).not.toMatch(/fallback/i);
          }
        }
      } finally {
        if (ref) await adapter.close(ref).catch(() => undefined);
        rmSync(dir, { recursive: true, force: true });
      }
    },
    20_000,
  );
});
