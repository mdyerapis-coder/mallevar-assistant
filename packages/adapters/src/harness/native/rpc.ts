import { createHash } from 'node:crypto';
import type { NativeProcess, ProbeResult } from './process';

/** A frame the adapter must act on: either a notification or a server-originated request. */
export type RpcStreamItem =
  | { kind: 'notification'; method: string; params: unknown }
  | { kind: 'server_request'; id: number; method: string; params: unknown };

/** Bounded in-memory frame queue, safe for one reader + bounded buffering. */
export class FrameQueue {
  #items: RpcStreamItem[] = [];
  #waiters: Array<{ resolve: (value: RpcStreamItem) => void; reject: (reason: unknown) => void }> = [];
  #closed: { reason: string } | undefined;
  push(frame: RpcStreamItem): void {
    if (this.#closed) return;
    const waiter = this.#waiters.shift();
    if (waiter) { waiter.resolve(frame); return; }
    if (this.#items.length >= 4_096) {
      this.#items.length = 0;
      this.close('rpc_queue_overflow');
      throw new Error('rpc_queue_overflow');
    }
    this.#items.push(frame);
  }
  close(reason: string): void {
    if (this.#closed) return;
    this.#closed = { reason };
    for (const waiter of this.#waiters.splice(0)) waiter.reject(new Error(reason));
  }
  async take(timeoutMs: number, signal?: AbortSignal): Promise<RpcStreamItem> {
    signal?.throwIfAborted();
    const item = this.#items.shift();
    if (item) return item;
    if (this.#closed) throw new Error(this.#closed.reason);
    const { promise, resolve, reject } = Promise.withResolvers<RpcStreamItem>();
    const waiter = { resolve, reject };
    this.#waiters.push(waiter);
    const abort = () => {
      const index = this.#waiters.indexOf(waiter);
      if (index !== -1) {
        this.#waiters.splice(index, 1);
        reject(signal?.reason ?? new Error('rpc_read_aborted'));
      }
    };
    signal?.addEventListener('abort', abort, { once: true });
    const timer = setTimeout(() => {
      const index = this.#waiters.indexOf(waiter);
      if (index !== -1) { this.#waiters.splice(index, 1); reject(new Error('rpc_request_timeout')); }
    }, timeoutMs);
    timer.unref?.();
    try { return await promise; } finally {
      clearTimeout(timer);
      signal?.removeEventListener('abort', abort);
    }
  }
  get closedReason(): string | undefined { return this.#closed?.reason; }
}

export class RpcError extends Error {
  constructor(public readonly code: number, message: string, public readonly data?: unknown) {
    super(message);
  }
}

type PendingRequest = { resolve: (result: unknown) => void; reject: (reason: unknown) => void; timer: NodeJS.Timeout };

/** Client half of a newline-delimited JSON-RPC 2.0 channel.
 *  Responses resolve `request()`; notifications and server-originated requests (id + method) are
 *  surfaced uniformly through `queue`. Server requests must be answered by the caller via `reply`
 *  (fail-closed deny for unrecognised requests is the caller's responsibility). */
export class JsonRpcClient {
  #nextId = 0;
  #pending = new Map<number, PendingRequest>();
  readonly queue = new FrameQueue();
  #process: NativeProcess;
  #reader: Promise<void>;
  #failure?: Error;
  #fail(reason: string): void {
    this.#failure ??= new Error(reason);
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(this.#failure);
    }
    this.#pending.clear();
    this.queue.close(reason);
  }

  constructor(process: NativeProcess) {
    this.#process = process;
    this.#reader = this.#pump();
  }
  #pump(): Promise<void> {
    return (async () => {
      for await (const frame of this.#process.frames()) {
        if (frame && typeof frame === 'object') {
          const obj = frame as Record<string, unknown>;
          const id = obj['id'];
          if (typeof id === 'number') {
            if ('method' in obj) {
              this.queue.push({ kind: 'server_request', id, method: String(obj['method']), params: obj['params'] });
              continue;
            }
            const pending = this.#pending.get(id);
            if (pending) {
              this.#pending.delete(id);
              clearTimeout(pending.timer);
              if ('error' in obj && obj['error'] && typeof obj['error'] === 'object') {
                const err = obj['error'] as { code?: number; message?: string; data?: unknown };
                pending.reject(new RpcError(typeof err.code === 'number' ? err.code : -1, String(err.message ?? 'rpc_error'), err.data));
              } else {
                pending.resolve(obj['result']);
              }
              continue;
            }
          } else if ('method' in obj) {
            this.queue.push({ kind: 'notification', method: String(obj['method']), params: obj['params'] });
            continue;
          }
        }
        // Non-RPC frame (e.g. a bare log line): surface as a notification with an empty method.
        this.queue.push({ kind: 'notification', method: '', params: frame });
      }
      this.#fail('native_stream_closed');
    })().catch(() => {
      this.#fail('native_stream_error');
    });
  }
  request(method: string, params: unknown, timeoutMs = 15_000): Promise<unknown> {
    if (this.#failure) return Promise.reject(this.#failure);
    const { promise, resolve, reject } = Promise.withResolvers<unknown>();
    const id = ++this.#nextId;
    const timer = setTimeout(() => {
      this.#pending.delete(id);
      reject(new Error(`rpc_request_timeout:${method}`));
    }, timeoutMs);
    timer.unref?.();
    this.#pending.set(id, { resolve, reject, timer });
    this.#process.writeFrame({ jsonrpc: '2.0', id, method, params }).catch(error => {
      this.#pending.delete(id);
      clearTimeout(timer);
      reject(error);
    });
    return promise;
  }
  notify(method: string, params: unknown): Promise<void> {
    return this.#process.writeFrame({ jsonrpc: '2.0', method, params });
  }
  /** Reply to a server-originated request with a JSON-RPC result (no method). */
  async reply(id: number, result: unknown): Promise<void> {
    await this.#process.writeFrame({ jsonrpc: '2.0', id, result });
  }
  /** Reply to a server-originated request with a JSON-RPC error (no method). */
  async replyError(id: number, code: number, message: string): Promise<void> {
    await this.#process.writeFrame({ jsonrpc: '2.0', id, error: { code, message } });
  }
  async close(): Promise<void> {
    this.#fail('rpc_closed');
    await this.#process.kill(false);
    await this.#reader.catch(() => undefined);
  }
}

export function object(value: unknown): Record<string, unknown> {
  if (typeof value === 'object' && value !== null) return value as Record<string, unknown>;
  throw new Error('NativeObjectExpected');
}

export function firstLine(text: string): string {
  const line = text.split('\n').map(part => part.trim()).find(part => part.length > 0);
  return line ?? '';
}

export function versionFromProbe(probe: ProbeResult, patterns: RegExp[]): string {
  const stdout = probe.stdout.trim();
  const stderr = probe.stderr.trim();
  for (const pattern of patterns) {
    const match = stdout.match(pattern) ?? stderr.match(pattern);
    if (match?.[1] !== undefined) return match[1];
  }
  return '';
}

export function sha256Hex(text: string): string {
  return createHash('sha256').update(text).digest('hex');
}
