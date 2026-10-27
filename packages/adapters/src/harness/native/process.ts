import { spawn, type ChildProcess } from 'node:child_process';
import { decodeFrames } from '../../rpc';

export const PROBE_TIMEOUT_MS = 10_000;
export const RPC_TIMEOUT_MS = 15_000;
export const CLOSE_TIMEOUT_MS = 15_000;
/** Managed native home root; C3 containment can override this per dispatch. */
export const NATIVE_STATE_ROOT = '/var/lib/ironbark/native';
/** Isolated worker environment for managed native processes. */
export function workerEnv(overrides: Record<string, string> = {}): Record<string, string> {
  return {
    PATH: process.env.PATH ?? '/usr/local/bin:/usr/bin:/bin',
    LANG: process.env.LANG ?? 'C.UTF-8',
    LC_ALL: process.env.LC_ALL ?? 'C.UTF-8',
    ...overrides,
  };
}
export type ProbeResult = { exit: number | null; signal: NodeJS.Signals | null; stdout: string; stderr: string };
export type SpawnOptions = { cwd: string; env?: Record<string, string>; stderrSink?: (chunk: string) => void };
export type NativeProcess = {
  readonly args: string[];
  readonly pid: number | undefined;
  readonly stdin: NodeJS.WritableStream;
  frames(): AsyncIterable<unknown>;
  writeFrame(frame: unknown): Promise<void>;
  kill(force: boolean): Promise<void>;
  exit: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;
};
export function runProbe(binary: string, args: string[], options: SpawnOptions, timeoutMs = PROBE_TIMEOUT_MS): Promise<ProbeResult> {
  const child = spawn(binary, args, { stdio: ['ignore', 'pipe', 'pipe'], cwd: options.cwd, env: { ...workerEnv(), ...(options.env ?? {}) } });
  const { promise, resolve } = Promise.withResolvers<ProbeResult>();
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', chunk => { stdout += chunk.toString(); if (stdout.length > 65_536) child.kill('SIGKILL'); });
  child.stderr.on('data', chunk => {
    stderr += chunk.toString();
    if (stderr.length > 65_536) child.kill('SIGKILL');
    if (options.stderrSink) options.stderrSink(chunk.toString());
  });
  child.once('error', error => { clearTimeout(timer); resolve({ exit: null, signal: null, stdout, stderr: `${stderr}\nspawn_error: ${error.message}` }); });
  const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs);
  child.once('close', (code, signal) => { clearTimeout(timer); resolve({ exit: code, signal, stdout, stderr }); });
  return promise;
}
/** One newline-delimited JSON-RPC process with frame-level byte limits. */
export function spawnNativeProcess(binary: string, args: string[], options: SpawnOptions): NativeProcess {
  const child: ChildProcess = spawn(binary, args, { stdio: ['pipe', 'pipe', 'pipe'], cwd: options.cwd, env: { ...workerEnv(), ...(options.env ?? {}) } });
  const stdout = child.stdout;
  if (!stdout) throw new Error('NativeProcessMissingStdout');
  const stdin = child.stdin;
  if (!stdin) throw new Error('NativeProcessMissingStdin');
  child.stderr?.on('data', chunk => {
    if (options.stderrSink) options.stderrSink(chunk.toString());
  });
  let dead = false;
  let killPromise: Promise<void> | undefined;
  const kill = (force: boolean) => {
    if (killPromise) return killPromise;
    const { promise, resolve } = Promise.withResolvers<void>();
    killPromise = promise;
    if (dead) { resolve(); return killPromise; }
    child.once('exit', () => { dead = true; resolve(); });
    child.kill(force ? 'SIGKILL' : 'SIGTERM');
    setTimeout(() => { dead = true; child.kill('SIGKILL'); }, CLOSE_TIMEOUT_MS).unref();
    return killPromise;
  };
  const writeFrame = (frame: unknown) => {
    if (dead) return Promise.reject(new Error('NativeProcessClosed'));
    const { promise, resolve, reject } = Promise.withResolvers<void>();
    const bytes = new TextEncoder().encode(JSON.stringify(frame) + '\n');
    if (bytes.length > 8 * 1024 * 1024) { reject(new Error('rpc_frame_oversize')); return promise; }
    if (stdin.write(bytes)) resolve();
    else {
      stdin.once('drain', resolve);
      stdin.once('error', reject);
      stdin.once('close', () => reject(new Error('NativeProcessClosed')));
    }
    return promise;
  };
  const exit = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>(resolve => {
    child.once('close', (code, signal) => { dead = true; resolve({ code, signal }); });
    child.once('error', () => { dead = true; resolve({ code: null, signal: null }); });
  });
  return {
    args,
    get pid() { return child.pid; },
    get stdin() { return stdin; },
    frames() { return decodeFrames(stdout); },
    writeFrame,
    kill,
    exit,
  };
}
export function describeExit(code: number | null, signal: NodeJS.Signals | null): string {
  return signal ? `signal=${signal}` : `exit=${code ?? 'unknown'}`;
}