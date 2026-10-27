// B1 test-only Bun process. Never selectable by a production feature/profile.
import { closeSync, fstatSync, openSync, readdirSync, statSync, writeFileSync, writeSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { dlopen, FFIType } from 'bun:ffi';

const libc = dlopen('libc.so.6', {
  prctl: { args: [FFIType.i32, FFIType.u64, FFIType.u64, FFIType.u64, FFIType.u64], returns: FFIType.i32 },
  fcntl: { args: [FFIType.i32, FFIType.i32, FFIType.i32], returns: FFIType.i32 },
});
const controlIdentity = fstatSync(6);
const credentialIdentity = fstatSync(3);
const protectedFiles = [[controlIdentity.dev, controlIdentity.ino], [credentialIdentity.dev, credentialIdentity.ino]];
const binary = process.argv[2];
const stateDir = process.argv[3];
const evidence = process.argv[4];
if (!binary || !stateDir || !evidence) throw new Error('B1FixtureArgumentsRequired');
const mark = (stage: string): void => { try { const fd = openSync(`${evidence}.stage`, 'a'); writeSync(fd, stage + '\n'); closeSync(fd); } catch { /* diagnostics only */ } };

try {
  // This is the production seam under test, not fixture-authored prctl or fd flags.
  // The consumer must harden BEFORE reading either inherited private transport.
  // Intentional module-loading-boundary test: static adapter imports would run
  // before the security bootstrap whose ordering this fixture exercises.
  const { openManagedHarnessChannel, serveHarnessRpc, createHarnessPermissionHost } = await import('../../src/harness/worker');
  process.on('exit', code => mark(`exit:${code}`));
  process.on('uncaughtException', error => { const stack = error instanceof Error ? (error.stack ?? '').split('\n').slice(0, 4).join(' | ').slice(0, 200) : ''; mark(`uncaught:${error instanceof Error ? (error.message.startsWith('worker_') ? error.message : `other:${error.message.slice(0,120)}`) : 'unknown'}:${stack}`); });
  process.on('unhandledRejection', error => mark(`rejection:${error instanceof Error ? (error.message.startsWith('worker_') ? error.message : `other:${error.message.slice(0,120)}`) : 'unknown'}`));
  mark('channel-opened');
  const { channel, credentials } = await openManagedHarnessChannel();
  if (libc.symbols.prctl(3, 0, 0, 0, 0) !== 0) throw new Error('ManagedWorkerStillDumpable');
  if (libc.symbols.prctl(39, 0, 0, 0, 0) !== 1) throw new Error('ManagedWorkerMissingNoNewPrivileges');
  if (libc.symbols.prctl(21, 0, 0, 0, 0) !== 2) throw new Error('ManagedWorkerMissingSeccompFilter');
  if ((libc.symbols.fcntl(6, 1, 0) & 1) !== 1) throw new Error('ManagedChannelMissingCloseOnExec');
  if (new TextDecoder().decode(credentials) !== 'private-test-credential') throw new Error('CredentialBootstrapMismatch');
  credentials.fill(0);
  if (process.env.IRONBARK_B1_DOTENV_TRAP !== undefined) throw new Error('UntrustedDotenvLoadedBeforeBootstrap');
  // Probe open-file identities rather than fd numbers: Bun may legitimately reuse fd3.
  for (const entry of readdirSync('/proc/self/fd')) {
    if (entry === '6') continue;
    try {
      const stat = statSync(`/proc/self/fd/${entry}`);
      if (stat.dev === credentialIdentity.dev && stat.ino === credentialIdentity.ino) throw new Error('CredentialBootstrapStillOpen');
    } catch (error) {
      if (!(error instanceof Error) || !('code' in error) || error.code !== 'ENOENT') throw error;
    }
  }
  const trustedToolConfig = `${evidence}.bunfig`;
  writeFileSync(trustedToolConfig, '', { mode: 0o400 });
  const tool = spawn(process.execPath, ['--no-env-file', '--no-install', `--config=${trustedToolConfig}`, '-e', `
    const { readdirSync, statSync, writeFileSync } = require('node:fs');
    const protectedFiles = ${JSON.stringify(protectedFiles)};
    let failure;
    try {
      for (const fd of readdirSync('/proc/self/fd')) {
        let stat;
        try { stat = statSync('/proc/self/fd/' + fd); } catch { continue; }
        if (protectedFiles.some(([dev, ino]) => stat.dev === dev && stat.ino === ino)) { failure = 'inherited_channel_fd'; break; }
      }
      if (!failure) {
        const { dlopen, FFIType } = require('bun:ffi');
        const native = dlopen('libc.so.6', {
          prctl: { args: [FFIType.i32, FFIType.u64, FFIType.u64, FFIType.u64, FFIType.u64], returns: FFIType.i32 },
          ptrace: { args: [FFIType.i32, FFIType.i32, FFIType.ptr, FFIType.ptr], returns: FFIType.i64 },
        });
        const nnp = native.symbols.prctl(39, 0, 0, 0, 0); const seccomp = native.symbols.prctl(21, 0, 0, 0, 0);
        const traced = Number(native.symbols.ptrace(0, 0, null, null));
        if (nnp !== 1 || seccomp !== 2 || traced !== -1) failure = 'hardening_not_inherited:' + nnp + ':' + seccomp + ':' + traced;
      }
    } catch (error) {
      failure = 'tool_script_error:' + (error && (error.code || error.message || 'unknown'));
    }
    if (failure === undefined) writeFileSync(${JSON.stringify(evidence)}, 'private-identities-absent\\n');
    else writeFileSync(${JSON.stringify(evidence)} + '.tool', failure + '\\n');
  `], { env: { PATH: '/usr/bin:/bin', HOME: process.cwd() }, cwd: process.cwd(), stdio: 'ignore' });
  const toolExit = await new Promise<number | null>((resolve, reject) => { tool.once('error', () => reject(new Error('tool_spawn_failed'))); tool.once('exit', resolve); });
  if (toolExit !== 0) throw new Error(`tool_exit_${toolExit}`);
  // Import actual native adaptation only AFTER the production bootstrap barrier.
  const { OpenCodeAdapter } = await import('../../src/harness/opencode');
  mark('adapter-imported');
  const host = createHarnessPermissionHost(channel);
  mark('host-created');
  await serveHarnessRpc(channel, nativeHost => new OpenCodeAdapter(nativeHost, { binary, stateDir }), host);
  mark('serve-returned');
} catch (error) {
  // Only an allowlisted content-free code may be retained, never exception data.
  const channelFlags = libc.symbols.fcntl(6, 1, 0);
  const failureCode = error instanceof Error && (error.message === 'worker_credential_oversize' || error.message === 'tool_spawn_failed' || error.message.startsWith('tool_exit_')) ? error.message : 'worker_failed';
  const detail = failureCode === 'worker_failed' && error instanceof Error && !error.message.startsWith('worker_')
    ? `:${error.message.slice(0, 160)}`.replace(/[\r\n]/g, ' ') : '';
  writeFileSync(`${evidence}.failure`, JSON.stringify({
    dumpable: libc.symbols.prctl(3, 0, 0, 0, 0),
    no_new_privs: libc.symbols.prctl(39, 0, 0, 0, 0),
    seccomp: libc.symbols.prctl(21, 0, 0, 0, 0),
    channel_closed_or_cloexec: channelFlags === -1 || (channelFlags & 1) === 1,
    failure: failureCode + detail,
  }));
  mark('caught');
  process.exitCode = 71;
  await new Promise<void>(resolve => setTimeout(resolve, 5_000));
} finally { libc.close(); }
