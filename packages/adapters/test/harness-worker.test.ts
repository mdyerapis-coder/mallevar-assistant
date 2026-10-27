import { describe, expect, it } from 'bun:test';
import { spawn } from 'node:child_process';
import type { ChildProcess } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, watch, writeFileSync } from 'node:fs';
import type { FSWatcher } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Duplex } from 'node:stream';
import type { OpenSessionInput, SessionRef } from '../../contracts/src/generated';

type Frame = Record<string, unknown>;
type Exit = { code: number | null; signal: NodeJS.Signals | null };
type Fixture = {
  root: string;
  cwd: string;
  nativeLog: string;
  evidence: string;
  child: ChildProcess;
  exit: Promise<Exit>;
  peer: Peer;
  credentials: Duplex;
  input: OpenSessionInput;
  diagnostics(): string;
};

// Actual executable ACP fixture, not a HarnessAdapter mock or echo server. The
// native prompt stays incomplete until a real cancel or permission reply.
const NATIVE = `#!${process.execPath}
import { appendFileSync, existsSync, writeFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
const log = process.argv[1] + '.log';
const emit = value => process.stdout.write(JSON.stringify(value) + '\\n');
const record = value => appendFileSync(log, JSON.stringify(value) + '\\n');
if (process.argv.includes('--version')) { console.log('opencode 0.19.0'); process.exit(0); }
const lines = createInterface({ input: process.stdin });
let pending;
let mode;
lines.on('close', () => process.exit(0));
lines.on('line', line => {
  const msg = JSON.parse(line);
  record(msg);
  if (msg.method === 'initialize') emit({jsonrpc:'2.0',id:msg.id,result:{protocolVersion:1,agentInfo:{name:'opencode',version:'fixture-only'}}});
  else if (msg.method === 'session/new') emit({jsonrpc:'2.0',id:msg.id,result:{sessionId:'fixture-native-session',models:{availableModels:[]}}});
  else if (msg.method === 'session/prompt') {
    pending = msg.id;
    const prompt = JSON.stringify(msg.params.prompt);
    mode = prompt.includes('flood') ? 'flood' : prompt.includes('permission') ? 'permission' : 'hold';
    emit({jsonrpc:'2.0',method:'session/update',params:{sessionId:'fixture-native-session',update:{sessionUpdate:'agent_message_chunk',content:{type:'text',text:'private-partial'}}}});
    if (prompt.includes('unmatched-tool')) {
      emit({jsonrpc:'2.0',method:'session/update',params:{sessionId:'fixture-native-session',update:{
        sessionUpdate:'tool_call',toolCallId:'unprepared-tool',name:'file_write',kind:'edit',status:'in_progress',
        rawInput:{path:process.argv[1]+'.effect',content_digest:'c'.repeat(64)},
      }}});
    }
    if (mode === 'flood') {
      const update = {jsonrpc:'2.0',method:'session/update',params:{sessionId:'fixture-native-session',update:{sessionUpdate:'agent_message_chunk',content:{type:'text',text:'q'.repeat(600000)}}}};
      // Bounded producer: a stuck private consumer must stop this worker, not
      // finish successfully or buffer an unbounded stream in the JS bridge.
      let count = 0;
      const send = () => { while (count < 64) { count++; if (!process.stdout.write(JSON.stringify(update)+'\\n')) { process.stdout.once('drain',send); return; } } };
      send();
    } else if (mode === 'permission') {
      emit({jsonrpc:'2.0',id:901,method:'session/request_permission',params:{
        sessionId:'fixture-native-session',
        toolCall:{toolCallId:'tool-permission-1',name:'file_write',kind:'edit',rawInput:{path:process.argv[1]+'.effect',content_digest:'c'.repeat(64)}},
        options:[{optionId:'yes',kind:'allow_once'},{optionId:'no',kind:'reject_once'}],
      }});
    }
  } else if (msg.method === 'session/cancel') {
    if (pending !== undefined) { emit({jsonrpc:'2.0',id:pending,result:{stopReason:'cancelled'}}); pending = undefined; }
  } else if (msg.method === 'session/load') {
    if (existsSync(process.argv[1]+'.export')) {
      emit({jsonrpc:'2.0',method:'session/update',params:{sessionId:'fixture-native-session',update:{sessionUpdate:'user_message_chunk',content:{type:'text',text:'native replay, not supplied context'}}}});
      emit({jsonrpc:'2.0',id:msg.id,result:{}});
    }
    // Otherwise leave requests pending to exercise actual control admission.
  } else if (msg.id === 901 && msg.result) {
    if (msg.result.outcome?.optionId === 'yes') writeFileSync(process.argv[1]+'.effect', 'must-not-be-written');
    if (pending !== undefined) { emit({jsonrpc:'2.0',id:pending,result:{stopReason:'end_turn'}}); pending = undefined; }
  }
});
`;

function record(value: unknown): Frame {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new Error('ExpectedPrivateObject');
  return value as Frame;
}
function packet(frame: Frame): Buffer {
  const body = Buffer.from(JSON.stringify(frame));
  const header = Buffer.alloc(4); header.writeUInt32BE(body.byteLength);
  return Buffer.concat([header, body]);
}
class Peer {
  private readonly input: AsyncIterator<unknown>;
  private bytes = Buffer.alloc(0);
  constructor(readonly socket: Duplex, readonly run: string) { this.input = socket[Symbol.asyncIterator](); }
  async write(bytes: Uint8Array): Promise<void> {
    await new Promise<void>((resolve, reject) => this.socket.write(bytes, error => error ? reject(error) : resolve()));
  }
  async send(method: Frame, id = crypto.randomUUID(), run = this.run): Promise<string> {
    await this.write(packet({ protocol: 2, kind: 'request', id, run_id: run, method }));
    return id;
  }
  async read(): Promise<Frame> {
    for (;;) {
      if (this.bytes.length >= 4) {
        const length = this.bytes.readUInt32BE();
        if (length === 0 || length > 1_048_576) throw new Error('InvalidPrivateFrameLength');
        if (this.bytes.length >= length + 4) {
          const body = this.bytes.subarray(4, length + 4);
          this.bytes = this.bytes.subarray(length + 4);
          return record(JSON.parse(body.toString('utf8')));
        }
      }
      const next = await this.input.next();
      if (next.done) throw new Error('PrivateEOF');
      if (!(next.value instanceof Uint8Array)) throw new Error('ExpectedSocketBytes');
      this.bytes = Buffer.concat([this.bytes, next.value]);
    }
  }
  async until(predicate: (frame: Frame) => boolean): Promise<Frame> {
    for (;;) {
      const frame = await this.read();
      expect(frame['protocol']).toBe(2);
      expect(frame['run_id']).toBe(this.run);
      if (predicate(frame)) return frame;
      if (frame['kind'] === 'host_request') {
        // The peer only answers non-authorizing state/audit callbacks. Positive
        // permission authority is exercised with the real Rust Store companion,
        // never manufactured by this transport adversary.
        const method = record(frame['method']);
        let result: Frame;
        if (method['type'] === 'record_native_event') result = { type: 'record_native_event' };
        else if (method['type'] === 'state') result = { type: 'state', state: { policy_version: '1', state_digest: 'd'.repeat(64) } };
        else throw new Error('UnexpectedAuthorizingHostCallback');
        await this.write(packet({ protocol: 2, kind: 'host_reply', id: frame['id'], request_id: frame['request_id'], run_id: this.run, session_id: frame['session_id'], result }));
      } else if (frame['kind'] !== 'event') throw new Error(`UnexpectedPrivateFrame:${String(frame['kind'])}`);
    }
  }
  async reply(id: string): Promise<Frame> { return this.until(frame => frame['kind'] === 'reply' && frame['id'] === id); }
}

async function withWorker(use: (fixture: Fixture) => Promise<void>, options: { credential?: Uint8Array; holdCredentialEof?: boolean } = {}): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), 'ironbark-b1-worker-'));
  const cwd = join(root, 'workspace');
  const state = join(root, 'private-state');
  const native = join(root, 'native-fixture.mjs');
  const config = join(root, 'trusted-bunfig.toml');
  const evidence = join(root, 'descriptor-proof');
  mkdirSync(cwd, { mode: 0o700 }); mkdirSync(state, { mode: 0o700 });
  writeFileSync(native, NATIVE, { mode: 0o700 });
  writeFileSync(`${native}.log`, '', { mode: 0o600 });
  writeFileSync(config, '', { mode: 0o400 });
  const entry = fileURLToPath(new URL('./fixtures/harness-worker-process.ts', import.meta.url));
  // This is transport/bootstrap test setup, not a second production argv
  // authority. The Rust ManagedBunLaunch namespace test owns launch-policy proof.
  const args = ['--no-env-file', '--no-install', `--config=${config}`, entry, native, state, evidence];
  const child = spawn(process.execPath, args, {
    cwd, env: { PATH: '/usr/bin:/bin', HOME: state, XDG_CONFIG_HOME: state, XDG_CACHE_HOME: state, XDG_DATA_HOME: state },
    stdio: ['ignore', 'pipe', 'pipe', 'pipe', 'ignore', 'ignore', 'pipe'],
  });
  const exit = new Promise<Exit>((resolve, reject) => { child.once('error', reject); child.once('exit', (code, signal) => resolve({ code, signal })); });
  const channel = child.stdio.at(6);
  const credentials = child.stdio[3];
  if (!(channel instanceof Duplex) || !(credentials instanceof Duplex)) throw new Error('MissingPrivateSocketpair');
  credentials.on('error', error => {
    // A bounded reader can close before a hostile oversized writer drains.
    // Unexpected bootstrap transport errors still fail the private channel.
    if ((options.credential?.byteLength ?? 0) <= 16_384) channel.destroy(error);
  });
  let diagnostics = '';
  for (const stream of [child.stdout, child.stderr]) stream?.on('data', (bytes: Buffer) => { diagnostics = (diagnostics + bytes.toString()).slice(-8192); });
  const run = '11111111-1111-4111-8111-111111111111';
  const project = '22222222-2222-4222-8222-222222222222';
  const input: OpenSessionInput = {
    run_id: run, project_id: project, cwd,
    route: { connection_id: 'fixture-local', model_id: 'fixture', harness: 'opencode' },
    context: { digest: 'a'.repeat(64), brief: '', constraints: [], excerpts: [], artifacts: [], unresolved_questions: [], originating_sessions: [] },
    policy: { version: '1', project_id: project, allowed_connection_ids: ['fixture-local'], allowed_tools: ['file_write'], read_roots: [cwd], write_roots: [cwd], network_destinations: [], local_only: true, max_workers: 1, max_requests: 4, spend_policy: 'free_only', allow_unknown_cost: false },
  };
  try {
    if (options.holdCredentialEof) credentials.write(options.credential ?? Buffer.from('private-test-credential'));
    else credentials.end(options.credential ?? Buffer.from('private-test-credential'));
    await use({ root, cwd, nativeLog: `${native}.log`, evidence, child, exit, peer: new Peer(channel, run), credentials, input, diagnostics: () => diagnostics });
  } finally {
    channel.destroy(); credentials.destroy();
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
    await exit;
    rmSync(root, { force: true, recursive: true });
  }
}
async function openSession(fixture: Fixture): Promise<SessionRef> {
  const hello = await fixture.peer.send({ type: 'hello' });
  expect(record((await fixture.peer.reply(hello))['result'])).toEqual({ type: 'hello', protocol: 2 });
  const describe = await fixture.peer.send({ type: 'describe' });
  const descriptor = record(record((await fixture.peer.reply(describe))['result'])['descriptor']);
  expect(descriptor['approval']).toBe('unqualified');
  const opened = await fixture.peer.send({ type: 'open', input: fixture.input });
  const result = record((await fixture.peer.reply(opened))['result']);
  const ref = record(result['session']);
  expect(ref['native_id']).toBe('fixture-native-session');
  expect(ref['project_id']).toBe(fixture.input.project_id);
  expect(ref['cwd']).toBe(fixture.cwd);
  expect(ref['resolved_route']).toEqual(fixture.input.route);
  if (ref['harness'] !== 'opencode' || typeof ref['native_id'] !== 'string' || typeof ref['native_version'] !== 'string') throw new Error('InvalidSessionReply');
  return { harness: 'opencode', native_id: ref['native_id'], native_version: ref['native_version'], project_id: fixture.input.project_id, cwd: fixture.cwd, resolved_route: fixture.input.route };
}
function turn(fixture: Fixture, session: SessionRef, text = 'hold'): Frame {
  return { type: 'turn', session, input: { request_id: crypto.randomUUID(), message_id: '33333333-3333-4333-8333-333333333333', content: [{ type: 'text', text }], allowed_tools: [{ name: 'file_write', description: '', input_schema: {} }], budget: fixture.input.policy } };
}
async function nativeLoads(fixture: Fixture, count: number): Promise<void> {
  // Bun's fs.watch coalesces burst writes (measured ~1 callback per 130 rapid
  // appends), so the watcher alone cannot observe a fast native flood. A
  // bounded poll of the log is the authoritative signal; watch only accelerates
  // the first read. Cross-process file IPC emits no awaitable event, so a
  // wall-clock poll (not fake timers) is the correct mechanism here.
  let watcher: FSWatcher | undefined;
  let poll: NodeJS.Timeout | undefined;
  await new Promise<void>((resolve, reject) => {
    let settled = false;
    const finish = (error?: Error): void => {
      if (settled) return;
      settled = true;
      clearInterval(poll);
      watcher?.close();
      if (error) reject(error); else resolve();
    };
    const observe = (): void => {
      try {
        const frames = readFileSync(fixture.nativeLog, 'utf8').trim().split('\n').filter(Boolean).map(line => record(JSON.parse(line)));
        if (frames.filter(frame => frame['method'] === 'session/load').length >= count) finish();
      } catch {
        // A burst write can be read mid-line; the next poll/fs event re-reads.
      }
    };
    watcher = watch(fixture.root, observe);
    poll = setInterval(observe, 25);
    fixture.exit.then(() => finish(new Error('WorkerExitedBeforeNativeLoad')), () => finish(new Error('WorkerExitWatchFailed')));
    observe();
  });
}

describe('B1 managed private worker — not native/provider qualification', () => {
  it('services interrupt while a real native prompt is unfinished', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      const active = await fixture.peer.send(turn(fixture, session));
      const partial = await fixture.peer.until(frame => frame['kind'] === 'event' && record(record(frame['event'])['payload'])['type'] === 'text_delta');
      expect(record(record(partial['event'])['payload'])['delta']).toBe('private-partial');
      const interrupt = await fixture.peer.send({ type: 'interrupt', session });
      const seen = new Map<string, Frame>();
      let terminal: Frame | undefined;
      while (!seen.has(active) || !seen.has(interrupt) || !terminal) {
        const frame = await fixture.peer.read();
        if (frame['kind'] === 'reply') seen.set(String(frame['id']), frame);
        else if (frame['kind'] === 'event') terminal = record(record(frame['event'])['payload']);
        else throw new Error('UnexpectedInterruptFrame');
      }
      expect(terminal['status']).toBe('cancelled');
      expect(seen.get(active)?.['error']).toBeUndefined();
      expect(seen.get(interrupt)?.['error']).toBeUndefined();
      const native = readFileSync(fixture.nativeLog, 'utf8').split('\n').filter(Boolean).map(line => record(JSON.parse(line)));
      expect(native.some(frame => frame['method'] === 'session/cancel')).toBe(true);
      expect(readFileSync(fixture.evidence, 'utf8')).toBe('private-identities-absent\n');
    });
  }, 15_000);

  it('refuses cross-run/session requests and unknown methods before native prompt effect', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      const wrongSession = await fixture.peer.send(turn(fixture, { ...session, native_id: 'foreign' }));
      expect(record((await fixture.peer.reply(wrongSession))['error'])['code']).toBe('worker_correlation');
      const wrongRun = await fixture.peer.send(turn(fixture, session), crypto.randomUUID(), '44444444-4444-4444-8444-444444444444');
      const rejected = await fixture.peer.read();
      expect(rejected['id']).toBe(wrongRun);
      expect(record(rejected['error'])['code']).toBe('worker_correlation');
      const unknown = await fixture.peer.send({ type: 'execute_arbitrary', command: 'touch outside' });
      expect(record((await fixture.peer.reply(unknown))['error'])['code']).toBe('worker_method');
      const native = readFileSync(fixture.nativeLog, 'utf8');
      expect(native).not.toContain('session/prompt');
    });
  }, 15_000);

  it('rejects an uncorrelated host state reply without dispatching a native prompt', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      const active = await fixture.peer.send(turn(fixture, session));
      const callback = await fixture.peer.until(frame => frame['kind'] === 'host_request' && record(frame['method'])['type'] === 'state');
      expect(callback['session_id']).toBe(session.native_id);
      expect(callback['request_id']).toBe(active);
      await fixture.peer.write(packet({ protocol: 2, kind: 'host_reply', id: crypto.randomUUID(), request_id: active, run_id: fixture.input.run_id, session_id: session.native_id, result: { type: 'state', state: { policy_version: '1', state_digest: 'd'.repeat(64) } } }));
      // Wrong callback identity poisons the channel: no indefinite host await.
      const result = await fixture.exit;
      expect(result.code).not.toBe(0);
      expect(readFileSync(fixture.nativeLog, 'utf8')).not.toContain('session/prompt');
      expect(fixture.diagnostics()).not.toContain('private-test-credential');
    });
  }, 15_000);

  it('exports native replay and closes the actual session over the private channel', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      writeFileSync(join(fixture.root, 'native-fixture.mjs.export'), 'release-native-history');
      const exported = await fixture.peer.send({ type: 'export_context', session });
      const context = record(record((await fixture.peer.reply(exported))['result'])['context']);
      expect(context['brief']).toBe('user: native replay, not supplied context');
      expect(context['originating_sessions']).toEqual([session]);
      const closed = await fixture.peer.send({ type: 'close', session });
      expect(record((await fixture.peer.reply(closed))['result'])).toEqual({ type: 'close' });
    });
  }, 15_000);

  it.each([1, 3])('rejects private protocol %i without launching a native process', async protocol => {
    await withWorker(async fixture => {
      await fixture.peer.write(packet({ protocol, kind: 'request', id: crypto.randomUUID(), run_id: fixture.input.run_id, method: { type: 'hello' } }));
      expect((await fixture.exit).code).not.toBe(0);
      expect(readFileSync(fixture.nativeLog, 'utf8')).toBe('');
    });
  }, 15_000);

  it('denies an unsolicited permission answer without native permission dispatch', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      const id = await fixture.peer.send({ type: 'answer_permission', session, answer: {
        native_request_id: 'invented-permission', decision: 'allow',
        action: { id: crypto.randomUUID(), run_id: fixture.input.run_id, project_id: fixture.input.project_id, kind: 'file_write', canonical_args: {}, scope: {}, policy_version: '1', expected_state_digest: 'd'.repeat(64) },
        grant: { id: crypto.randomUUID(), action_id: crypto.randomUUID(), action_digest: 'b'.repeat(64), policy_version: '1', expected_state_digest: 'd'.repeat(64), expires_at: `${Date.now() + 60_000}` },
      } });
      expect(record((await fixture.peer.reply(id))['error'])['code']).toBe('worker_permission_denied');
      expect(readFileSync(fixture.nativeLog, 'utf8')).not.toContain('session/prompt');
    });
  }, 15_000);

  it('forwards a real native permission to core and sends a native deny when core refuses', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      const active = await fixture.peer.send(turn(fixture, session, 'permission'));
      const callback = await fixture.peer.until(frame => frame['kind'] === 'host_request' && record(frame['method'])['type'] === 'prepare_action');
      expect(callback['request_id']).toBe(active);
      expect(callback['session_id']).toBe(session.native_id);
      const request = record(record(callback['method'])['request']);
      expect(request['request_id']).toBe('901');
      expect(request['tool_call_id']).toBe('tool-permission-1');
      expect(request['name']).toBe('acp.toolPermission');
      await fixture.peer.write(packet({ protocol: 2, kind: 'host_reply', id: callback['id'], request_id: active, run_id: fixture.input.run_id, session_id: session.native_id, error: { code: 'worker_permission_denied' } }));
      expect(record((await fixture.peer.reply(active))['result'])).toEqual({ type: 'turn' });
      const frames = readFileSync(fixture.nativeLog, 'utf8').trim().split('\n').map(line => record(JSON.parse(line)));
      const denied = frames.find(frame => frame['id'] === 901 && frame['result'] !== undefined);
      expect(record(record(denied?.['result'])['outcome'])['optionId']).toBe('no');
      expect(existsSync(join(fixture.root, 'native-fixture.mjs.effect'))).toBe(false);
    });
  }, 15_000);

  it('terminates on a correlated core correlation failure even when the native adapter catches it', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      const active = await fixture.peer.send(turn(fixture, session, 'permission'));
      const callback = await fixture.peer.until(frame => frame['kind'] === 'host_request' && record(frame['method'])['type'] === 'prepare_action');
      await fixture.peer.write(packet({ protocol: 2, kind: 'host_reply', id: callback['id'], request_id: active, run_id: fixture.input.run_id, session_id: session.native_id, error: { code: 'worker_correlation' } }));
      expect((await fixture.exit).code).not.toBe(0);
      expect(existsSync(join(fixture.root, 'native-fixture.mjs.effect'))).toBe(false);
      expect(fixture.diagnostics()).not.toContain('private-test-credential');
    });
  }, 15_000);

  it.each([
    null,
    { type: 'state', state: { policy_version: '1', state_digest: 'd'.repeat(64), forged_authority: true } },
    { type: 'verify_answer', verified: true },
  ])('fails closed on a malformed or wrong-variant state callback result (%#)', async result => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      const active = await fixture.peer.send(turn(fixture, session));
      const callback = await fixture.peer.until(frame => frame['kind'] === 'host_request' && record(frame['method'])['type'] === 'state');
      await fixture.peer.write(packet({ protocol: 2, kind: 'host_reply', id: callback['id'], request_id: active, run_id: fixture.input.run_id, session_id: session.native_id, result }));
      expect((await fixture.exit).code).not.toBe(0);
      expect(readFileSync(fixture.nativeLog, 'utf8')).not.toContain('session/prompt');
    });
  }, 15_000);

  it('rejects replay of a settled callback while its real native turn remains active', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      const active = await fixture.peer.send(turn(fixture, session));
      const callback = await fixture.peer.until(frame => frame['kind'] === 'host_request' && record(frame['method'])['type'] === 'state');
      const reply = packet({ protocol: 2, kind: 'host_reply', id: callback['id'], request_id: active, run_id: fixture.input.run_id, session_id: session.native_id, result: { type: 'state', state: { policy_version: '1', state_digest: 'd'.repeat(64) } } });
      await fixture.peer.write(reply);
      await fixture.peer.until(frame => frame['kind'] === 'event' && record(record(frame['event'])['payload'])['type'] === 'text_delta');
      await fixture.peer.write(reply);
      expect((await fixture.exit).code).not.toBe(0);
      expect(readFileSync(fixture.nativeLog, 'utf8')).toContain('session/prompt');
    });
  }, 15_000);

  it('dispatches coalesced controls through the single framed reader', async () => {
    await withWorker(async fixture => {
      const hello = crypto.randomUUID();
      const describe = crypto.randomUUID();
      await fixture.peer.write(Buffer.concat([
        packet({ protocol: 2, kind: 'request', id: hello, run_id: fixture.input.run_id, method: { type: 'hello' } }),
        packet({ protocol: 2, kind: 'request', id: describe, run_id: fixture.input.run_id, method: { type: 'describe' } }),
      ]));
      const replies = new Map<string, Frame>();
      while (replies.size < 2) {
        const frame = await fixture.peer.read();
        expect(frame['kind']).toBe('reply');
        replies.set(String(frame['id']), record(frame['result']));
      }
      expect(replies.get(hello)).toEqual({ type: 'hello', protocol: 2 });
      expect(replies.get(describe)?.['type']).toBe('describe');
      expect(readFileSync(fixture.nativeLog, 'utf8')).toBe('');
    });
  }, 15_000);

  it('rejects a route harness of the wrong JSON type before native open', async () => {
    await withWorker(async fixture => {
      const hello = await fixture.peer.send({ type: 'hello' });
      await fixture.peer.reply(hello);
      const opened = await fixture.peer.send({ type: 'open', input: { ...fixture.input, route: { ...fixture.input.route, harness: ['opencode'] } } });
      expect(record((await fixture.peer.reply(opened))['error'])['code']).toBe('worker_method');
      expect(readFileSync(fixture.nativeLog, 'utf8')).toBe('');
    });
  }, 15_000);

  it('rejects actual native tool telemetry with no core-prepared and verified permission', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      await fixture.peer.send(turn(fixture, session, 'unmatched-tool'));
      await fixture.peer.until(frame => frame['kind'] === 'event' && record(record(frame['event'])['payload'])['type'] === 'text_delta');
      expect((await fixture.exit).code).not.toBe(0);
      expect(existsSync(join(fixture.root, 'native-fixture.mjs.effect'))).toBe(false);
      expect(readFileSync(fixture.nativeLog, 'utf8')).toContain('session/prompt');
    });
  }, 15_000);

  it('waits for credential EOF before accepting even a buffered hello', async () => {
    await withWorker(async fixture => {
      const id = await fixture.peer.send({ type: 'hello' });
      let replied = false;
      const response = fixture.peer.reply(id).then(frame => { replied = true; return frame; });
      // Cross-process integration: fake JS timers cannot drive the worker's
      // inherited pipe/EOF. This window checks that no pre-EOF reply escapes.
      await new Promise<void>(resolve => setTimeout(resolve, 150));
      expect(replied).toBe(false);
      expect(readFileSync(fixture.nativeLog, 'utf8')).toBe('');
      fixture.credentials.end();
      expect(record((await response)['result'])).toEqual({ type: 'hello', protocol: 2 });
    }, { holdCredentialEof: true });
  }, 15_000);

  it.each([
    Buffer.from([0xff]),
    Buffer.from('{'),
    Buffer.from(JSON.stringify({ protocol: 2, kind: 'request', id: 'not-a-uuid', run_id: '11111111-1111-4111-8111-111111111111', method: { type: 'hello' } })),
    Buffer.from(JSON.stringify({ protocol: 2, kind: 'request', id: '55555555-5555-4555-8555-555555555555', run_id: '11111111-1111-4111-8111-111111111111', method: { type: 'hello' }, forged_authority: true })),
  ])('terminates on malformed private bytes without native dispatch (%#)', async body => {
    await withWorker(async fixture => {
      const header = Buffer.alloc(4); header.writeUInt32BE(body.length);
      await fixture.peer.write(Buffer.concat([header, body]));
      expect((await fixture.exit).code).not.toBe(0);
      expect(readFileSync(fixture.nativeLog, 'utf8')).toBe('');
      expect(fixture.diagnostics()).not.toContain('private-test-credential');
    });
  }, 15_000);

  it('admits at most 128 pending controls without forwarding the 129th to native', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      for (let index = 0; index < 128; index++) await fixture.peer.send({ type: 'export_context', session });
      await nativeLoads(fixture, 128);
      const overflow = await fixture.peer.send({ type: 'export_context', session });
      expect(record((await fixture.peer.reply(overflow))['error'])['code']).toBe('worker_busy');
      const native = readFileSync(fixture.nativeLog, 'utf8').trim().split('\n').map(line => record(JSON.parse(line)));
      expect(native.filter(frame => frame['method'] === 'session/load')).toHaveLength(128);
    });
  }, 15_000);

  it.each([0, 1_048_577])('rejects invalid frame length %i before reading an attacker-sized body', async length => {
    await withWorker(async fixture => {
      await openSession(fixture);
      const header = Buffer.alloc(4); header.writeUInt32BE(length);
      await fixture.peer.write(header);
      expect((await fixture.exit).code).not.toBe(0);
      expect(fixture.diagnostics()).not.toContain('private-test-credential');
    });
  }, 15_000);

  it('accepts split frame headers/bodies and rejects a truncated frame at EOF', async () => {
    await withWorker(async fixture => {
      const id = crypto.randomUUID();
      const bytes = packet({ protocol: 2, kind: 'request', id, run_id: fixture.input.run_id, method: { type: 'hello' } });
      await fixture.peer.write(bytes.subarray(0, 1));
      await fixture.peer.write(bytes.subarray(1, 7));
      await fixture.peer.write(bytes.subarray(7));
      expect(record((await fixture.peer.reply(id))['result'])['protocol']).toBe(2);
      await fixture.peer.write(Buffer.from([0, 0, 0, 100, 123]));
      fixture.peer.socket.end();
      expect((await fixture.exit).code).not.toBe(0);
    });
  }, 15_000);

  it('terminates a stalled private writer at 30 seconds instead of buffering the native flood', async () => {
    await withWorker(async fixture => {
      const session = await openSession(fixture);
      await fixture.peer.send(turn(fixture, session, 'flood'));
      await fixture.peer.until(frame => frame['kind'] === 'event' && record(record(frame['event'])['payload'])['type'] === 'text_delta');
      const start = performance.now();
      // Deliberately do not consume the private socket. This is real kernel
      // backpressure, not a fake clock, resolved writer, or forwarding echo.
      const result = await fixture.exit;
      const elapsed = performance.now() - start;
      expect(result.code).not.toBe(0);
      expect(elapsed).toBeGreaterThanOrEqual(29_000);
      expect(elapsed).toBeLessThan(38_000);
      expect(fixture.diagnostics()).not.toContain('private-test-credential');
    });
  }, 45_000);

  it('rejects oversized credential bootstrap only after native hardening, before native dispatch', async () => {
    await withWorker(async fixture => {
      expect((await fixture.exit).code).not.toBe(0);
      expect(JSON.parse(readFileSync(`${fixture.evidence}.failure`, 'utf8'))).toEqual({
        dumpable: 0, no_new_privs: 1, seccomp: 2, channel_closed_or_cloexec: true, failure: 'worker_credential_oversize',
      });
      expect(readFileSync(fixture.nativeLog, 'utf8')).toBe('');
      expect(fixture.diagnostics()).not.toContain('private-test-credential');
    }, { credential: Buffer.alloc(16_385, 120) });
  }, 15_000);
});
