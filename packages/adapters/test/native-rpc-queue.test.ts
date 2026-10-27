import { expect, test } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { FrameQueue, JsonRpcClient, type RpcStreamItem } from '../src/harness/native/rpc';
import { spawnNativeProcess } from '../src/harness/native/process';

const notification: RpcStreamItem = { kind: 'notification', method: 'session/update', params: { text: 'next-turn' } };

test('a_cancelled_read_cannot_consume_the_next_turns_native_event', async () => {
  const queue = new FrameQueue();
  const controller = new AbortController();
  const abandoned = queue.take(10_000, controller.signal);
  controller.abort(new Error('turn_consumption_stopped'));
  queue.push(notification);
  await expect(abandoned).rejects.toThrow('turn_consumption_stopped');
  expect(await queue.take(10_000)).toEqual(notification);
});

test('an_already_aborted_read_leaves_buffered_native_events_untouched', async () => {
  const queue = new FrameQueue();
  const controller = new AbortController();
  queue.push(notification);
  controller.abort(new Error('turn_consumption_stopped'));
  await expect(queue.take(10_000, controller.signal)).rejects.toThrow('turn_consumption_stopped');
  expect(await queue.take(10_000)).toEqual(notification);
});

test('overflow_fails_the_native_stream_instead_of_silently_losing_permission_requests', async () => {
  const queue = new FrameQueue();
  for (let i = 0; i < 4_096; i++) queue.push(notification);
  expect(() => queue.push({ kind: 'server_request', id: 1, method: 'session/request_permission', params: {} }))
    .toThrow('rpc_queue_overflow');
  await expect(queue.take(10_000)).rejects.toThrow('rpc_queue_overflow');
});

test('native_exit_rejects_every_pending_request_as_disconnect_not_a_fabricated_timeout', async () => {
  const home = mkdtempSync(join(tmpdir(), 'ironbark-rpc-eof-'));
  const native = spawnNativeProcess(process.execPath, ['--eval', 'for await (const _ of Bun.stdin.stream()) { process.exit(23); }'], {
    cwd: home, env: { HOME: home, XDG_CONFIG_HOME: home },
  });
  const client = new JsonRpcClient(native);
  try {
    // This is the real request deadline, not a readiness sleep: an EOF must
    // report disconnection rather than wait until that deadline misclassifies it.
    const pending = ['first', 'second', 'third'].map(method =>
      client.request(method, {}, 2_000).then(value => ({ value }), error => ({ error })));
    expect((await native.exit).code).toBe(23);
    await expect(client.queue.take(10_000)).rejects.toThrow('native_stream_closed');
    for (const result of await Promise.all(pending)) {
      expect('error' in result).toBe(true);
      if ('error' in result) {
        expect(result.error).toBeInstanceOf(Error);
        expect(result.error.message).not.toContain('timeout');
      }
    }
  } finally {
    await client.close();
    rmSync(home, { recursive: true, force: true });
  }
});
