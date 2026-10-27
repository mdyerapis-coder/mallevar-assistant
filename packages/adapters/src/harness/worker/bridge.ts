import { AsyncLocalStorage } from 'node:async_hooks';
import { randomUUID } from 'node:crypto';
import type { ActionSpec, OpenSessionInput, SessionRef, TurnInput, WorkerEvent } from '../../../../contracts/src/generated';
import type { NativePermissionHost, NativePermissionRequest } from '../native/host';
import type { HarnessAdapter } from '../types';
import type { DuplexChannel, HarnessFactory } from '../worker';
import { WorkerFault, closedObject, contextPackage, harnessDescriptor, hostResult, incomingFrame, nativeEvent, nativeMethod, permissionAnswer, sameJson, sessionRef } from './protocol';
import type { HostMethod, HostReplyFrame, HostResult, IncomingFrame, NativeMethod, NativeResult, OutgoingFrame, RequestFrame, WorkerCode } from './protocol';

const FRAME_LIMIT = 1_048_576;
const PENDING_LIMIT = 128;
const WRITE_STALL_MS = 30_000;
// Bounded replay history for a session; saturation fails closed rather than
// forgetting a native identity and allowing its reuse in a later turn.
const TOOL_HISTORY_LIMIT = 4_096;

type Control = { id: string; run: string; method: NativeMethod; nativeTurn?: string; terminal?: boolean };
type PendingCallback = {
  control: Control;
  session: string;
  method: HostMethod;
  done: Promise<HostResult>;
  settled: boolean;
  resolve(result: HostResult): void;
  reject(error: Error): void;
};
type PermissionBinding = {
  request: NativePermissionRequest;
  action: ActionSpec;
  turn: Control;
  verifying: boolean;
  answered: boolean;
  allowed: boolean;
  started: boolean;
  finished: boolean;
};

/** Detach and freeze core/control values before exposing them to an adapter.
 * In-flight native work must not mutate the identity later checked by replies.
 */
function immutable<T>(value: T): T {
  const copy = structuredClone(value);
  const seen = new WeakSet<object>();
  const freeze = (item: unknown): void => {
    if (typeof item !== 'object' || item === null || seen.has(item)) return;
    seen.add(item);
    for (const child of Object.values(item)) freeze(child);
    Object.freeze(item);
  };
  freeze(copy);
  return copy;
}

/** This is correlation of an understood operation, not policy evaluation.
 * The core alone prepares/persists the action and consumes the stored grant.
 */
function fileWriteArgs(request: NativePermissionRequest): Record<string, unknown> {
  let value: unknown = request.input;
  if (request.name === 'acp.toolPermission') {
    const wrapper = closedObject(value, ['name', 'rawInput'], ['title', 'kind']);
    if (wrapper['name'] !== 'file_write') throw new WorkerFault('worker_permission_denied');
    value = wrapper['rawInput'];
  } else if (request.name !== 'file_write') throw new WorkerFault('worker_permission_denied');
  const args = closedObject(value, ['path', 'content_digest']);
  if (typeof args['path'] !== 'string' || !args['path'] || typeof args['content_digest'] !== 'string' || !/^[0-9a-f]{64}$/.test(args['content_digest'])) throw new WorkerFault('worker_permission_denied');
  return args;
}

/** One bounded queue and one monotonic deadline per active kernel write. */
class OrderedWriter {
  #tail: Promise<void> = Promise.resolve();
  #count = 0;
  #bytes = 0;
  #failed = false;
  constructor(readonly channel: DuplexChannel, readonly fail: () => void) {}
  send(frame: OutgoingFrame): Promise<void> {
    if (this.#failed) return Promise.reject(new WorkerFault('worker_correlation'));
    const json = JSON.stringify(frame);
    const length = Buffer.byteLength(json);
    if (length === 0 || length > FRAME_LIMIT || this.#count >= PENDING_LIMIT || this.#bytes + length + 4 > 2 * FRAME_LIMIT) {
      this.#failed = true; this.fail();
      return Promise.reject(new WorkerFault('worker_busy'));
    }
    const packet = Buffer.allocUnsafe(length + 4);
    packet.writeUInt32BE(length); packet.write(json, 4);
    this.#count++; this.#bytes += packet.length;
    const operation = this.#tail.then(async () => {
      if (this.#failed) throw new WorkerFault('worker_correlation');
      const deadline = performance.now() + WRITE_STALL_MS;
      let timer: NodeJS.Timeout | undefined;
      const expired = new Promise<never>((_resolve, reject) => {
        const check = (): void => {
          const remaining = deadline - performance.now();
          if (remaining > 0) { timer = setTimeout(check, remaining); return; }
          reject(new WorkerFault('worker_correlation'));
        };
        timer = setTimeout(check, WRITE_STALL_MS);
      });
      try { await Promise.race([this.channel.write(packet), expired]); }
      finally { clearTimeout(timer); }
    }).catch(() => {
      this.#failed = true; this.fail(); throw new WorkerFault('worker_correlation');
    }).finally(() => { packet.fill(0); this.#count--; this.#bytes -= packet.length; });
    // The queue owns its rejection even when the originating adapter forgets
    // to await a failed callback. The caller still receives the rejected send.
    this.#tail = operation.catch(() => undefined);
    return operation;
  }
}

/** Read exactly one framed stream; header validation precedes body allocation. */
async function* frames(channel: DuplexChannel): AsyncIterable<IncomingFrame> {
  const header = Buffer.alloc(4);
  let headerUsed = 0;
  let body: Buffer | undefined;
  let bodyUsed = 0;
  const decoder = new TextDecoder('utf-8', { fatal: true });
  try {
    for await (const chunk of channel.input) {
      if (!(chunk instanceof Uint8Array)) throw new WorkerFault('worker_correlation');
      let offset = 0;
      while (offset < chunk.byteLength) {
        if (!body) {
          const count = Math.min(4 - headerUsed, chunk.byteLength - offset);
          header.set(chunk.subarray(offset, offset + count), headerUsed);
          headerUsed += count; offset += count;
          if (headerUsed !== 4) continue;
          const length = header.readUInt32BE();
          if (length === 0 || length > FRAME_LIMIT) throw new WorkerFault('worker_correlation');
          body = Buffer.allocUnsafe(length); bodyUsed = 0;
        }
        const count = Math.min(body.length - bodyUsed, chunk.byteLength - offset);
        body.set(chunk.subarray(offset, offset + count), bodyUsed);
        bodyUsed += count; offset += count;
        if (bodyUsed === body.length) {
          let value: unknown;
          try { value = JSON.parse(decoder.decode(body)); }
          finally { body.fill(0); body = undefined; headerUsed = 0; bodyUsed = 0; }
          yield incomingFrame(value);
        }
      }
    }
    if (body || headerUsed !== 0) throw new WorkerFault('worker_correlation');
  } finally { header.fill(0); body?.fill(0); }
}

/** A single dispatcher owns both control requests and callback replies.
 * ALS carries the originating live control through asynchronous adapter work;
 * no global "last request" can accidentally authorize a concurrent callback.
 */
export class HarnessBridge {
  readonly #context = new AsyncLocalStorage<Control>();
  readonly #controls = new Map<string, Control>();
  readonly #callbacks = new Map<string, PendingCallback>();
  readonly #permissions = new Map<string, PermissionBinding>();
  readonly #preparing = new Map<string, Control>();
  readonly #actions = new Map<string, PermissionBinding>();
  readonly #usedTools = new Map<string, true>();
  readonly #writer: OrderedWriter;
  readonly #failure = Promise.withResolvers<never>();
  #failed = false;
  #serving = false;
  #hello = false;
  #run?: string;
  #input?: OpenSessionInput;
  #session?: SessionRef;
  #openingNative?: string;
  #opening = false;
  #closing = false;
  #turn?: Control;
  #adapter?: HarnessAdapter;

  constructor(readonly channel: DuplexChannel) {
    this.#failure.promise.catch(() => undefined);
    this.#writer = new OrderedWriter(channel, () => this.#abort());
  }

  readonly host: NativePermissionHost = {
    state: async (input, turn) => {
      const control = this.#boundCallback(input);
      if (control.method.type !== 'turn' || control !== this.#turn || control.terminal || !turn || !sameJson(turn, control.method.input)) throw new WorkerFault('worker_correlation');
      const result = await this.#callback(control, { type: 'state', input, turn });
      if (result.type !== 'state') throw new WorkerFault('worker_correlation');
      return result.state;
    },
    prepareAction: async (request, input) => {
      request = immutable(request);
      const control = this.#boundCallback(input);
      if (control !== this.#turn || control.terminal || !this.#session || request.session_id !== this.#session.native_id || this.#permissions.has(request.request_id) || this.#preparing.has(request.request_id)) throw new WorkerFault('worker_permission_denied');
      closedObject(request, ['request_id', 'session_id', 'name', 'tool_call_id', 'input', 'raw']);
      if (!request.request_id || typeof request.request_id !== 'string' || typeof request.name !== 'string' || typeof request.tool_call_id !== 'string' || !request.tool_call_id || this.#usedTools.has(request.tool_call_id)) throw new WorkerFault('worker_permission_denied');
      const args = fileWriteArgs(request);
      if (this.#permissions.size + this.#preparing.size >= PENDING_LIMIT || this.#actions.size >= PENDING_LIMIT || this.#usedTools.size >= TOOL_HISTORY_LIMIT) throw new WorkerFault('worker_busy');
      this.#preparing.set(request.request_id, control);
      this.#usedTools.set(request.tool_call_id, true);
      try {
        const result = await this.#callback(control, { type: 'prepare_action', request, input });
        if (result.type !== 'prepare_action' || result.action.run_id !== this.#run || result.action.project_id !== input.project_id || result.action.kind !== 'file_write' || !sameJson(result.action.canonical_args, args)) { this.#abort(); throw new WorkerFault('worker_correlation'); }
        for (const issued of this.#actions.values()) if (issued.action.id === result.action.id) { this.#abort(); throw new WorkerFault('worker_correlation'); }
        const binding: PermissionBinding = { request, action: immutable(result.action), turn: control, verifying: false, answered: false, allowed: false, started: false, finished: false };
        this.#permissions.set(request.request_id, binding);
        this.#actions.set(request.tool_call_id, binding);
        return binding.action;
      } finally { this.#preparing.delete(request.request_id); }
    },
    verifyAnswer: async (answer, request) => {
      const control = this.#boundCallback();
      const pending = this.#permissions.get(request.request_id);
      if (control.method.type !== 'answer_permission' || !sameJson(control.method.answer, answer) || !pending || pending.verifying || pending.answered || pending.turn !== this.#turn || pending.turn.terminal || !sameJson(pending.request, request) || !sameJson(pending.action, answer.action) || answer.native_request_id !== request.request_id) throw new WorkerFault('worker_permission_denied');
      pending.verifying = true;
      try {
        const result = await this.#callback(control, { type: 'verify_answer', answer, request: pending.request });
        if (result.type !== 'verify_answer' || pending.turn !== this.#turn || pending.turn.terminal) throw new WorkerFault('worker_correlation');
        if (result.verified) {
          pending.answered = true;
          pending.allowed = answer.decision === 'allow';
          this.#permissions.delete(request.request_id);
        }
        return result.verified;
      } finally { pending.verifying = false; }
    },
    recordNativeEvent: async (event, input) => {
      event = immutable(event);
      const control = this.#boundCallback(input);
      if (control.method.type === 'open' && event['event'] === 'session_bound') {
        if (typeof event['native_id'] !== 'string' || !event['native_id'] || (input.route.harness && event['harness'] !== input.route.harness) || (this.#openingNative && this.#openingNative !== event['native_id'])) throw new WorkerFault('worker_correlation');
        this.#openingNative = event['native_id'];
      }
      for (const key of ['native_id', 'session_id']) {
        if (event[key] !== undefined && event[key] !== (this.#session?.native_id ?? this.#openingNative)) throw new WorkerFault('worker_correlation');
      }
      await this.#callback(control, { type: 'record_native_event', event, input });
    },
  };

  #abort(): void {
    if (this.#failed) return;
    this.#failed = true;
    const error = new WorkerFault('worker_correlation');
    for (const callback of this.#callbacks.values()) callback.reject(error);
    this.#callbacks.clear(); this.#permissions.clear(); this.#preparing.clear(); this.#actions.clear();
    this.#failure.reject(error);
    // Closing immediately unblocks the sole reader and tells the core to reap
    // its existing namespace lease; JS never asserts a stop receipt.
    void this.channel.close().catch(() => undefined);
  }

  #boundCallback(input?: OpenSessionInput): Control {
    const control = this.#context.getStore();
    if (this.#failed || !control || this.#controls.get(control.id) !== control || control.run !== this.#run || (input && (!this.#input || !sameJson(input, this.#input)))) throw new WorkerFault('worker_correlation');
    return control;
  }

  async #callback(control: Control, method: HostMethod): Promise<HostResult> {
    const session = this.#session?.native_id ?? this.#openingNative;
    if (!session || this.#callbacks.size >= PENDING_LIMIT || this.#controls.get(control.id) !== control) throw new WorkerFault('worker_correlation');
    method = immutable(method);
    const id = randomUUID();
    const response = Promise.withResolvers<HostResult>();
    response.promise.catch(() => undefined);
    this.#callbacks.set(id, { control, session, method, done: response.promise, settled: false, resolve: response.resolve, reject: response.reject });
    try {
      await this.#writer.send({ protocol: 2, kind: 'host_request', id, request_id: control.id, run_id: control.run, session_id: session, method });
      return await response.promise;
    } finally { this.#callbacks.delete(id); }
  }

  #hostReply(frame: HostReplyFrame): void {
    const pending = this.#callbacks.get(frame.id);
    if (!pending || pending.settled || frame.run_id !== this.#run || frame.request_id !== pending.control.id || frame.session_id !== pending.session || this.#controls.get(pending.control.id) !== pending.control) throw new WorkerFault('worker_correlation');
    pending.settled = true;
    if (frame.error) {
      pending.reject(new WorkerFault(frame.error.code));
      if (frame.error.code === 'worker_correlation') throw new WorkerFault('worker_correlation');
      return;
    }
    try { pending.resolve(immutable(hostResult(frame.result, pending.method.type))); }
    catch { pending.reject(new WorkerFault('worker_correlation')); throw new WorkerFault('worker_correlation'); }
  }

  async #reply(frame: RequestFrame, result?: NativeResult, error?: WorkerCode): Promise<void> {
    if (error) await this.#writer.send({ protocol: 2, kind: 'reply', id: frame.id, run_id: frame.run_id, error: { code: error } });
    else {
      if (!result) throw new WorkerFault('worker_correlation');
      await this.#writer.send({ protocol: 2, kind: 'reply', id: frame.id, run_id: frame.run_id, result });
    }
  }

  #request(frame: RequestFrame): void {
    if (this.#controls.has(frame.id) || this.#callbacks.has(frame.id)) throw new WorkerFault('worker_correlation');
    let method: NativeMethod;
    try { method = immutable(nativeMethod(frame.method)); }
    catch (error) {
      void this.#reply(frame, undefined, error instanceof WorkerFault ? error.code : 'worker_method').catch(() => this.#abort()); return;
    }
    if (this.#run && frame.run_id !== this.#run) { void this.#reply(frame, undefined, 'worker_correlation').catch(() => this.#abort()); return; }
    if ((!this.#hello && method.type !== 'hello') || (this.#hello && method.type === 'hello')) { void this.#reply(frame, undefined, 'worker_correlation').catch(() => this.#abort()); return; }
    if (this.#controls.size >= PENDING_LIMIT) { void this.#reply(frame, undefined, 'worker_busy').catch(() => this.#abort()); return; }
    if (method.type === 'hello') { this.#run = frame.run_id; this.#hello = true; }
    if (method.type === 'open') {
      if (this.#opening || this.#session || this.#closing) { void this.#reply(frame, undefined, 'worker_busy').catch(() => this.#abort()); return; }
      if (method.input.run_id !== frame.run_id) { void this.#reply(frame, undefined, 'worker_correlation').catch(() => this.#abort()); return; }
      this.#opening = true; this.#input = structuredClone(method.input);
    }
    if ('session' in method && (!this.#session || !sameJson(method.session, this.#session))) { void this.#reply(frame, undefined, 'worker_correlation').catch(() => this.#abort()); return; }
    if ('session' in method && this.#closing) { void this.#reply(frame, undefined, 'worker_busy').catch(() => this.#abort()); return; }
    const control: Control = { id: frame.id, run: frame.run_id, method };
    if (method.type === 'turn') {
      if (this.#turn || this.#permissions.size || !this.#input || method.input.budget.project_id !== this.#input.project_id) { void this.#reply(frame, undefined, 'worker_busy').catch(() => this.#abort()); return; }
      this.#turn = control;
    }
    if (method.type === 'close') this.#closing = true;
    this.#controls.set(frame.id, control);
    void this.#context.run(control, async () => {
      try {
        const result = await this.#execute(control);
        // Native audit callbacks may be initiated asynchronously while the
        // stream is active. Keep their originating control live through the
        // core acknowledgements, rather than racing its terminal reply.
        for (;;) {
          const pending: Array<Promise<HostResult>> = [];
          for (const callback of this.#callbacks.values()) if (callback.control === control) pending.push(callback.done);
          if (pending.length === 0) break;
          await Promise.all(pending);
        }
        if (!this.#failed) await this.#reply(frame, result);
      } catch (error) {
        if (!this.#failed) await this.#reply(frame, undefined, error instanceof WorkerFault ? error.code : (method.type === 'answer_permission' ? 'worker_permission_denied' : 'worker_method'));
      } finally {
        this.#controls.delete(frame.id);
        if (method.type === 'open') this.#opening = false;
        if (this.#turn === control) { this.#turn = undefined; this.#permissions.clear(); this.#preparing.clear(); this.#actions.clear(); }
        if (method.type === 'close') this.#closing = false;
        // A callback cannot outlive the native operation that originated it.
        for (const pending of this.#callbacks.values()) if (pending.control === control) { this.#abort(); break; }
      }
    }).catch(() => this.#abort());
  }

  async #execute(control: Control): Promise<NativeResult> {
    const adapter = this.#adapter;
    if (!adapter) throw new WorkerFault('worker_correlation');
    const method = control.method;
    switch (method.type) {
      case 'hello': return { type: 'hello', protocol: 2 };
      case 'describe': return { type: 'describe', descriptor: harnessDescriptor(await adapter.describe()) };
      case 'open': {
        const opened = immutable(sessionRef(await adapter.open(method.input)));
        this.#session = opened;
        if (opened.project_id !== method.input.project_id || opened.cwd !== method.input.cwd || (method.input.route.harness && opened.harness !== method.input.route.harness) || !sameJson(opened.resolved_route, method.input.route) || (this.#openingNative && opened.native_id !== this.#openingNative)) { this.#abort(); throw new WorkerFault('worker_correlation'); }
        if (this.#failed) { await adapter.close(opened).catch(() => undefined); throw new WorkerFault('worker_correlation'); }
        return { type: 'open', session: opened };
      }
      case 'turn': {
        for await (const event of adapter.turn(method.session, method.input)) {
          if (this.#failed) throw new WorkerFault('worker_correlation');
          let linked: WorkerEvent;
          try { linked = this.#linkToolEvent(control, immutable(event)); }
          catch { this.#abort(); throw new WorkerFault('worker_correlation'); }
          this.#event(control, method.input, linked);
          await this.#writer.send({ protocol: 2, kind: 'event', id: control.id, run_id: control.run, event: linked });
        }
        if (!control.terminal) { this.#abort(); throw new WorkerFault('worker_correlation'); }
        return { type: 'turn' };
      }
      case 'answer_permission': {
        const answer = permissionAnswer(method.answer);
        const pending = this.#permissions.get(answer.native_request_id);
        if (!pending || pending.turn !== this.#turn || pending.turn.terminal || pending.verifying || pending.answered || !sameJson(pending.action, answer.action) || answer.action.run_id !== control.run || answer.action.project_id !== this.#session?.project_id) throw new WorkerFault('worker_permission_denied');
        await adapter.answerPermission(answer);
        if (!pending.answered) throw new WorkerFault('worker_permission_denied');
        return { type: 'answer_permission' };
      }
      case 'interrupt': await adapter.interrupt(method.session); return { type: 'interrupt' };
      case 'export_context': return { type: 'export_context', context: contextPackage(await adapter.exportContext(method.session)) };
      case 'close':
        await adapter.close(method.session);
        this.#session = undefined; this.#permissions.clear(); this.#actions.clear(); this.#usedTools.clear(); this.#openingNative = undefined;
        return { type: 'close' };
    }
  }

  #linkToolEvent(control: Control, event: WorkerEvent): WorkerEvent {
    if (event.payload.type !== 'tool_started' && event.payload.type !== 'tool_finished') return event;
    closedObject(event, ['turn_id', 'run_id', 'payload'], ['session_id', 'tool_id']);
    if (event.payload.type === 'tool_started') closedObject(event.payload, ['type', 'action']);
    else closedObject(event.payload, ['type', 'action_id', 'result']);
    const binding = event.tool_id ? this.#actions.get(event.tool_id) : undefined;
    if (!binding || binding.turn !== control || control !== this.#turn || binding.request.session_id !== this.#session?.native_id || binding.action.run_id !== control.run || !binding.answered || !binding.allowed || binding.finished) throw new WorkerFault('worker_permission_denied');
    // Native IDs/hashes/titles are not authority. Match the exact previously
    // core-prepared and core-verified operation/args inside this live turn.
    const args = fileWriteArgs(binding.request);
    if (binding.action.kind !== 'file_write' || !sameJson(binding.action.canonical_args, args)) throw new WorkerFault('worker_permission_denied');
    if (event.payload.type === 'tool_started') {
      if (binding.started) throw new WorkerFault('worker_permission_denied');
      const source = closedObject(event.payload.action, ['id', 'run_id', 'project_id', 'kind', 'canonical_args', 'scope', 'policy_version', 'expected_state_digest'], ['destination']);
      if (source['run_id'] !== control.run || source['project_id'] !== this.#session?.project_id) throw new WorkerFault('worker_permission_denied');
      if (!sameJson(event.payload.action, binding.action)) {
        if (binding.request.name !== 'acp.toolPermission') throw new WorkerFault('worker_permission_denied');
        // Existing ACP adapters encode observed args as canonical JSON text.
        // Decode only that exact closed operation shape; never parse prose or
        // authorize from the adapter's synthetic action ID.
        let observedArgs: unknown = source['canonical_args'];
        if (typeof observedArgs === 'string') {
          if (Buffer.byteLength(observedArgs) > FRAME_LIMIT) throw new WorkerFault('worker_permission_denied');
          try { observedArgs = JSON.parse(observedArgs); }
          catch { throw new WorkerFault('worker_permission_denied'); }
        }
        const observed = closedObject(observedArgs, ['name', 'rawInput'], ['title']);
        if (observed['name'] !== 'file_write' || !sameJson(observed['rawInput'], args)) throw new WorkerFault('worker_permission_denied');
      }
      binding.started = true;
      return { ...event, payload: { type: 'tool_started', action: structuredClone(binding.action) } };
    }
    if (!binding.started) throw new WorkerFault('worker_permission_denied');
    binding.finished = true;
    // Native tool completion remains informational. It is not a verified
    // filesystem effect, independent check, or run acceptance observation.
    return { ...event, payload: { type: 'tool_finished', action_id: binding.action.id, result: event.payload.result } };
  }

  #event(control: Control, turn: TurnInput, event: WorkerEvent): void {
    try { nativeEvent(event); }
    catch { this.#abort(); throw new WorkerFault('worker_correlation'); }
    if (this.#turn !== control || this.#controls.get(control.id) !== control || event.run_id !== control.run || event.session_id !== this.#session?.native_id || control.terminal) { this.#abort(); throw new WorkerFault('worker_correlation'); }
    const payload = event.payload;
    if ('message_id' in payload && payload.message_id !== turn.message_id) { this.#abort(); throw new WorkerFault('worker_correlation'); }
    if (payload.type === 'provider_message' && !sameJson(payload.route, this.#session?.resolved_route)) { this.#abort(); throw new WorkerFault('worker_correlation'); }
    if (!control.nativeTurn) {
      if (payload.type !== 'turn_started' && event.turn_id !== turn.message_id) { this.#abort(); throw new WorkerFault('worker_correlation'); }
      control.nativeTurn = event.turn_id;
    } else if (event.turn_id !== control.nativeTurn || payload.type === 'turn_started') { this.#abort(); throw new WorkerFault('worker_correlation'); }
    if ('action' in payload) {
      let issued = false;
      for (const binding of this.#actions.values()) if (binding.turn === control && sameJson(binding.action, payload.action)) { issued = true; break; }
      if (payload.action.run_id !== control.run || payload.action.project_id !== this.#session?.project_id || !issued) { this.#abort(); throw new WorkerFault('worker_correlation'); }
      if (payload.type === 'approval_needed') {
        let found = false;
        for (const pending of this.#permissions.values()) if (pending.turn === control && sameJson(pending.action, payload.action)) { found = true; break; }
        if (!found) { this.#abort(); throw new WorkerFault('worker_correlation'); }
      }
    }
    if (payload.type === 'tool_finished') {
      const binding = event.tool_id ? this.#actions.get(event.tool_id) : undefined;
      if (!binding || !binding.finished || binding.action.id !== payload.action_id) { this.#abort(); throw new WorkerFault('worker_correlation'); }
    }
    if (payload.type === 'turn_finished') {
      if (this.#permissions.size || this.#preparing.size) { this.#abort(); throw new WorkerFault('worker_permission_denied'); }
      control.terminal = true;
    }
  }

  async serve(create: HarnessFactory, host: NativePermissionHost): Promise<void> {
    if (this.#serving) throw new WorkerFault('worker_correlation');
    this.#serving = true;
    try {
      this.#adapter = create(host);
      const reading = (async () => {
        for await (const frame of frames(this.channel)) {
          if (this.#failed) break;
          if (frame.kind === 'host_reply') this.#hostReply(frame); else this.#request(frame);
        }
        // Even a clean socket EOF withdraws the owned control channel, not a
        // fabricated successful native completion. Cleanup is always required.
        throw new WorkerFault('worker_correlation');
      })();
      await Promise.race([reading, this.#failure.promise]);
    } finally {
      this.#abort();
      let timer: NodeJS.Timeout | undefined;
      try {
        if (this.#session && this.#adapter) {
          await Promise.race([
            this.#adapter.close(this.#session).catch(() => undefined),
            new Promise<void>(resolve => { timer = setTimeout(resolve, 2_000); }),
          ]);
        }
      } finally {
        clearTimeout(timer); this.#controls.clear(); this.#callbacks.clear(); this.#permissions.clear(); this.#preparing.clear(); this.#actions.clear(); this.#usedTools.clear();
        this.#context.disable();
        await this.channel.close().catch(() => undefined);
      }
    }
  }
}
