// Trusted entry boundary: runtime imports here MUST remain Bun/Node builtins.
// In particular no adapter, SDK, plugin, or project module executes before hardening.
import { dlopen, FFIType, ptr } from 'bun:ffi';
import { closeSync, readSync, writeSync } from 'node:fs';
import { Socket } from 'node:net';
import type { NativePermissionHost } from './native/host';
import type { HarnessAdapter } from './types';
import type { HarnessBridge } from './worker/bridge';

export type DuplexChannel = {
  input: AsyncIterable<Uint8Array>;
  write(bytes: Uint8Array): Promise<void>;
  close(): Promise<void>;
};
export type HarnessFactory = (host: NativePermissionHost) => HarnessAdapter;

type ChannelBinding = { host?: NativePermissionHost; bridge?: HarnessBridge; managed: boolean };
const bindings = new WeakMap<DuplexChannel, ChannelBinding>();

/** Linux post-exec barrier, equivalent to harden_managed_process in the core.
 * TSYNC covers Bun's already-running runtime threads, not just this JS thread.
 */
export async function openManagedHarnessChannel(): Promise<{ channel: DuplexChannel; credentials: Uint8Array }> {
  let closeLibrary = (): void => undefined;
  const scratch = Buffer.alloc(16_385);
  let credentials: Uint8Array | undefined;
  let credentialOpen = true;
  let socket: Socket | undefined;
  try {
    if (process.platform !== 'linux' || (process.arch !== 'x64' && process.arch !== 'arm64')) throw new Error('worker_hardening');
    const libc = dlopen('libc.so.6', {
      prctl: { args: [FFIType.i32, FFIType.u64, FFIType.u64, FFIType.u64, FFIType.u64], returns: FFIType.i32 },
      fcntl: { args: [FFIType.i32, FFIType.i32, FFIType.i32], returns: FFIType.i32 },
      syscall: { args: [FFIType.i64, FFIType.u64, FFIType.u64, FFIType.ptr], returns: FFIType.i64 },
    });
    closeLibrary = () => libc.close();
    if (libc.symbols.prctl(4, 0, 0, 0, 0) !== 0 || libc.symbols.prctl(38, 1, 0, 0, 0) !== 0) throw new Error('worker_hardening');
    const filter: Array<[number, number, number, number]> = [
      [0x20, 0, 0, 4], [0x15, 1, 0, process.arch === 'x64' ? 0xc000003e : 0xc00000b7],
      [0x06, 0, 0, 0x80000000], [0x20, 0, 0, 0],
    ];
    if (process.arch === 'x64') filter.push([0x35, 0, 1, 0x40000000], [0x06, 0, 0, 0x80000000]);
    for (const nr of process.arch === 'x64' ? [101, 310, 311, 438] : [117, 270, 271, 438]) {
      filter.push([0x15, 0, 1, nr], [0x06, 0, 0, 0x00050001]);
    }
    filter.push([0x06, 0, 0, 0x7fff0000]);
    const instructions = Buffer.alloc(filter.length * 8);
    for (let index = 0; index < filter.length; index++) {
      const instruction = filter[index]!;
      instructions.writeUInt16LE(instruction[0], index * 8);
      instructions[index * 8 + 2] = instruction[1]; instructions[index * 8 + 3] = instruction[2];
      instructions.writeUInt32LE(instruction[3], index * 8 + 4);
    }
    const program = Buffer.alloc(16);
    program.writeUInt16LE(filter.length);
    program.writeBigUInt64LE(BigInt(ptr(instructions)), 8);
    // SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC. Any nonzero
    // result (including a failing thread id) fails closed.
    if (Number(libc.symbols.syscall(process.arch === 'x64' ? 317 : 277, 1, 1, ptr(program))) !== 0) throw new Error('worker_hardening');
    const flags = libc.symbols.fcntl(6, 1, 0);
    if (flags < 0 || libc.symbols.fcntl(6, 2, flags | 1) !== 0) throw new Error('worker_hardening');
    // The worker's fd 6 is a blocking socketpair endpoint. The fd channel
    // below must poll with EAGAIN backoff, so make the read side nonblocking.
    const streamFlags = libc.symbols.fcntl(6, 3, 0);
    if (streamFlags < 0 || libc.symbols.fcntl(6, 4, streamFlags | 0x800) !== 0) throw new Error('worker_hardening');
    const credentialFlags = libc.symbols.fcntl(3, 1, 0);
    if (credentialFlags < 0 || libc.symbols.fcntl(3, 2, credentialFlags | 1) !== 0) throw new Error('worker_hardening');
    let length = 0;
    for (;;) {
      let count: number;
      try { count = readSync(3, scratch, length, scratch.length - length, null); }
      catch (error) {
        if (error instanceof Error && 'code' in error && (error.code === 'EAGAIN' || error.code === 'EWOULDBLOCK')) {
          await new Promise<void>(resolve => setTimeout(resolve, 1)); continue;
        }
        throw new Error('worker_credential_read');
      }
      if (count === 0) break;
      length += count;
      if (length > 16_384) throw new Error('worker_credential_oversize');
    }
    closeSync(3); credentialOpen = false;
    credentials = Uint8Array.from(scratch.subarray(0, length));
    scratch.fill(0);
    // Bun 1.4.x cannot adopt an inherited socket via node:net (new Socket({fd})
    // leaves no handle and never reads). Drive fd 6 directly instead: nonblocking
    // readSync framed by EAGAIN backoff, and writeSync for output.
    socket = undefined;
    const channelFd = 6;
    const closeChannel = (): void => { try { closeSync(channelFd); } catch { /* already closed */ } };
    const input: AsyncIterable<Uint8Array> = {
      async *[Symbol.asyncIterator]() {
        const buffer = Buffer.alloc(16_384);
        for (;;) {
          let count: number;
          try { count = readSync(channelFd, buffer, 0, buffer.length, null); }
          catch (error) {
            if (error instanceof Error && 'code' in error && (error.code === 'EAGAIN' || error.code === 'EWOULDBLOCK')) {
              await new Promise<void>(resolve => setTimeout(resolve, 1)); continue;
            }
            const code = error instanceof Error && 'code' in error ? String(error.code) : 'unknown';
            throw new Error(`worker_channel_read:${code}`);
          }
          if (count === 0) return;
          yield Uint8Array.from(buffer.subarray(0, count));
        }
      },
    };
    const channel: DuplexChannel = {
      input,
      async write(bytes) {
        let offset = 0;
        while (offset < bytes.byteLength) {
          try { offset += writeSync(channelFd, bytes, offset, bytes.byteLength - offset); }
          catch (error) {
            if (error instanceof Error && 'code' in error && (error.code === 'EAGAIN' || error.code === 'EWOULDBLOCK')) {
              await new Promise<void>(resolve => setTimeout(resolve, 1)); continue;
            }
            throw new Error('worker_closed');
          }
        }
      },
      async close() { closeChannel(); },
    };
    bindings.set(channel, { managed: true });
    return { channel, credentials };
  } catch (error) {
    scratch.fill(0); credentials?.fill(0);
    if (credentialOpen) { try { closeSync(3); } catch { /* already closed */ } }
    if (socket) socket.destroy();
    throw new Error(error instanceof Error && error.message === 'worker_credential_oversize' ? 'worker_credential_oversize' : 'worker_hardening');
  } finally { scratch.fill(0); closeLibrary(); }
}

/** A channel-backed proxy only; all policy/action/grant decisions stay in Rust. */
export function createHarnessPermissionHost(channel: DuplexChannel): NativePermissionHost {
  let binding = bindings.get(channel);
  if (!binding) { binding = { managed: false }; bindings.set(channel, binding); }
  if (binding.host) return binding.host;
  const bound = binding;
  const bridge = (): HarnessBridge => {
    if (!bound.bridge) throw new Error('worker_correlation');
    return bound.bridge;
  };
  bound.host = {
    async state(input, turn) { return bridge().host.state(input, turn); },
    async prepareAction(request, input) { return bridge().host.prepareAction(request, input); },
    async verifyAnswer(answer, request) { return bridge().host.verifyAnswer(answer, request); },
    async recordNativeEvent(event, input) { return bridge().host.recordNativeEvent(event, input); },
  };
  return bound.host;
}

export async function serveHarnessRpc(channel: DuplexChannel, create: HarnessFactory, host: NativePermissionHost): Promise<void> {
  const binding = bindings.get(channel);
  if (!binding || binding.host !== host || binding.bridge) throw new Error('worker_correlation');
  try {
    // Deferred until the trusted bootstrap caller has crossed the barrier.
    const { HarnessBridge } = await import('./worker/bridge');
    binding.bridge = new HarnessBridge(channel);
    await binding.bridge.serve(create, host);
  } catch {
    // Import/construction failures need the same content-free EOF boundary as
    // failures from an already-running dispatcher.
    await channel.close().catch(() => undefined);
    if (binding.managed) {
      process.exitCode = 71;
      // Cleanup has already been attempted; the core observes socket EOF and
      // reaps its namespace lease. Do not leave this worker alive on SDK timers.
      setTimeout(() => process.exit(71), 0);
    }
    throw new Error('worker_failed');
  }
}
