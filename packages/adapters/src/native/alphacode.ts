import { decodeFrames, encodeFrame } from '../rpc.ts';
import type { RoutePin } from '../../../contracts/src/generated.ts';

export type AlphaIdentity = { buildDigest: string; effectiveRoute: RoutePin; cwd: string; allowedTools: string[]; profile: string; socket: string };
export type NativeAction = { session_id: string; tool_call_id: string; resolved_name: string; canonical_input: unknown; cwd: string; route: RoutePin; policy_version: string; state_digest: string };
export type NativePermission = { request_id: string; action: NativeAction };
export type StopObservation = { session_id: string; cancelled: boolean; active_tools: number; children: { terminated: boolean }[] };
export function canonical(value: unknown): string {
  if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
  if (value !== null && typeof value === 'object') return '{' + Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => JSON.stringify(k) + ':' + canonical(v)).join(',') + '}';
  const encoded = JSON.stringify(value); if (encoded === undefined) throw new Error('InvalidNativeIdentity'); return encoded;
}
export function validateAlphaIdentity(value: unknown, expected: AlphaIdentity): void {
  if (!value || typeof value !== 'object') throw new Error('UnpatchedAlphaCode');
  const identity = value as Record<string, unknown>;
  if (identity.permissionsVersion !== 1 || identity.gateActive !== true) throw new Error('UnpatchedAlphaCode');
  if (!/^[a-f0-9]{64}$/.test(expected.buildDigest) || Object.entries(expected).some(([key, v]) => canonical(identity[key]) !== canonical(v))) throw new Error('SessionIdentityMismatch');
}
export function permissionReply(request: NativePermission, answer: NativePermission & { allowed: boolean }) {
  if (request.request_id !== answer.request_id || canonical(request.action) !== canonical(answer.action)) throw new Error('PermissionIdentityMismatch');
  return { request_id: request.request_id, action: request.action, allowed: answer.allowed === true };
}
export function requireObservedStop(observation: StopObservation, session: string): void {
  if (observation.session_id !== session || observation.cancelled !== true || observation.active_tools !== 0 || !Array.isArray(observation.children) || observation.children.some(c => c.terminated !== true)) throw new Error('NativeChildrenNotObservedStopped');
}
export class AlphaTextState {
  #messages = new Map<string, string>();
  apply(kind: 'text_delta' | 'text_replace', message: string, text: string) { this.#messages.set(message, kind === 'text_replace' ? text : this.get(message) + text); }
  get(message: string) { return this.#messages.get(message) ?? ''; }
}
export type AlphaFrame = { jsonrpc: '2.0'; id?: string | number; method?: string; params?: unknown; result?: unknown; error?: { message?: string } };
export function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('InvalidAlphaObject');
  return value as Record<string, unknown>;
}
/** C3 owns both ACP and the dedicated daemon; this client never spawns a process. */
export type AlphaChannel = { source: AsyncIterable<Uint8Array>; write(frame: Uint8Array): Promise<void>; observeStop(session: string): Promise<StopObservation>; close(): Promise<void> };
export class AlphaNativeClient {
  #pending = new Map<string, { resolve(value: unknown): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> }>();
  #sink?: (frame: AlphaFrame) => void;
  #closed = false;
  #reader: Promise<void>;
  constructor(readonly channel: AlphaChannel, readonly identity: AlphaIdentity) {
    this.#reader = this.#read();
    // Errors are delivered to pending callers/active stream, never as unhandled rejections.
    this.#reader.catch(() => {});
  }
  async #read() {
    try {
      for await (const raw of decodeFrames(this.channel.source)) {
        const value = object(raw);
        if (value.jsonrpc !== '2.0' || (value.id !== undefined && typeof value.id !== 'string' && typeof value.id !== 'number') || (value.method !== undefined && typeof value.method !== 'string')) throw new Error('InvalidAlphaFrame');
        const error = value.error === undefined ? undefined : object(value.error);
        if (error && typeof error.message !== 'string') throw new Error('InvalidAlphaError');
        const frame: AlphaFrame = { jsonrpc: '2.0', id: value.id as string | number | undefined, method: value.method as string | undefined, params: value.params, result: value.result, error: error ? { message: error.message as string } : undefined };
        if (frame.method) {
          if (this.#sink) this.#sink(frame);
          else if (frame.id !== undefined) await this.reply(String(frame.id), { allowed: false });
        } else if (frame.id !== undefined) {
          const pending = this.#pending.get(String(frame.id));
          if (!pending) throw new Error('UncorrelatedAlphaReply');
          clearTimeout(pending.timer); this.#pending.delete(String(frame.id));
          frame.error ? pending.reject(new Error(frame.error.message ?? 'NativeAlphaError')) : pending.resolve(frame.result);
        }
      }
      throw new Error('AlphaCodeDisconnected');
    } catch (error) {
      this.#closed = true;
      const failure = error instanceof Error ? error : new Error('AlphaCodeDisconnected');
      for (const pending of this.#pending.values()) { clearTimeout(pending.timer); pending.reject(failure); }
      this.#pending.clear(); this.#sink?.({ jsonrpc: '2.0', method: '_ironbark/disconnected', params: { error: failure.message } });
    }
  }
  request(method: string, params: unknown, timeoutMs = 30_000): Promise<unknown> {
    if (this.#closed) return Promise.reject(new Error('AlphaCodeDisconnected'));
    const id = crypto.randomUUID();
    const { promise, resolve, reject } = Promise.withResolvers<unknown>();
    const timer = setTimeout(() => { this.#pending.delete(id); reject(new Error('AlphaRequestTimeout')); void this.channel.close(); }, timeoutMs);
    this.#pending.set(id, { resolve, reject, timer });
    this.channel.write(encodeFrame({ jsonrpc: '2.0', id, method, params })).catch(error => { clearTimeout(timer); this.#pending.delete(id); reject(error); });
    return promise;
  }
  async initialize() { const result = object(await this.request('initialize', { protocolVersion: 1 })); validateAlphaIdentity(result._ironbark, this.identity); return result; }
  async reply(id: string, result: unknown) { await this.channel.write(encodeFrame({ jsonrpc: '2.0', id, result })); }
  async *prompt(sessionId: string, prompt: unknown[]): AsyncIterable<AlphaFrame> {
    if (this.#sink) throw new Error('AlphaCodeTurnBusy');
    const queue: AlphaFrame[] = []; let wake: (() => void) | undefined; let finished = false; let failure: unknown;
    this.#sink = frame => { queue.push(frame); wake?.(); wake = undefined; };
    const run = this.request('session/prompt', { sessionId, prompt }, 3_600_000).then(result => { queue.push({ jsonrpc: '2.0', method: '_ironbark/turn_finished', params: result }); }, error => { failure = error; }).finally(() => { finished = true; wake?.(); wake = undefined; });
    try {
      while (!finished || queue.length) {
        if (queue.length) { const frame = queue.shift()!; if (frame.method === '_ironbark/disconnected') throw new Error(String(object(frame.params).error)); yield frame; }
        else { const { promise, resolve } = Promise.withResolvers<void>(); wake = resolve; await promise; }
      }
      if (failure) throw failure;
    } finally { this.#sink = undefined; if (!finished) { await this.request('session/cancel', { sessionId }).catch(() => {}); } await run; }
  }
  async interrupt(session: string) { await this.request('session/cancel', { sessionId: session }); requireObservedStop(await this.channel.observeStop(session), session); }
  async close() { await this.channel.close(); await this.#reader; }
}
