import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { HarnessKind } from '../../../contracts/src/generated';

export type FixtureMode = 'version' | 'failure' | 'malformed-version' | 'malformed-protocol';
export type FixtureInvocation = { pid: number; args: string[]; cwd: string; home: string };
export type NativeFixture = {
  root: string;
  binary: string;
  absentBinary: string;
  home: string;
  stateDir: string;
  cwd: string;
  invocations(): FixtureInvocation[];
};

const versions: Partial<Record<HarnessKind, string>> = {
  omp: 'omp/1.2.3',
  codex: 'codex-cli 1.2.3',
  opencode: 'opencode 1.2.3',
  claude: 'claude 1.2.3',
  hermes: 'Hermes Agent v1.2.3',
};

/** Test-only executable boundary, never a native capability qualification. */
export async function withNativeFixture(
  harness: HarnessKind,
  mode: FixtureMode,
  use: (fixture: NativeFixture) => Promise<void>,
): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), `ironbark-${harness}-unit-`));
  const home = join(root, 'home');
  const stateDir = join(root, 'state');
  const cwd = join(root, 'workspace');
  const binary = join(root, 'native-fixture.mjs');
  const log = join(root, 'invocations.jsonl');
  try {
    for (const path of [home, stateDir, cwd]) mkdirSync(path, { mode: 0o700 });
    writeFileSync(log, '', { mode: 0o600 });
    writeFileSync(binary, `#!${process.execPath}
import { appendFileSync, writeSync } from 'node:fs';
import { createInterface } from 'node:readline';
const home = ${JSON.stringify(home)};
// Version probes do not all pass HOME today. This executable cannot resolve
// an operator profile even if the adapter omits its managed environment.
process.env.HOME = home;
for (const name of ['XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'HERMES_HOME']) process.env[name] = home;
const args = process.argv.slice(2);
appendFileSync(${JSON.stringify(log)}, JSON.stringify({ pid: process.pid, args, cwd: process.cwd(), home }) + '\\n');
// Safety watchdog, not a test wait: broken cleanup cannot orphan a fixture.
// Stop/deadline tests must observe SIGKILL/SIGTERM, never this exit.
if (!args.includes('--version')) setTimeout(() => process.exit(70), 3000);
if (args.includes('--hang') || args.includes('--hold')) {
  console.log(JSON.stringify({ ready: true, pid: process.pid }));
} else if (args.includes('--version')) {
  if (${JSON.stringify(mode)} === 'failure') {
    writeSync(1, ${JSON.stringify(versions[harness])} + '\\n');
    writeSync(2, 'fixture version probe failed\\n');
    process.exit(23);
  }
  console.log(${JSON.stringify(mode === 'malformed-version' ? 'not-a-version' : versions[harness])});
} else if (${JSON.stringify(mode)} === 'malformed-protocol') {
  if (${JSON.stringify(harness)} === 'claude') {
    console.log('not-json');
    process.stdin.resume();
  } else {
    const lines = createInterface({ input: process.stdin });
    lines.on('line', line => {
      const request = JSON.parse(line);
      console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: null }));
    });
  }
} else {
  // A route-denial test reaching native dispatch is a real test failure.
  console.error('unexpected fixture protocol dispatch');
  process.exit(24);
}
`, { mode: 0o700 });
    chmodSync(root, 0o700);
    await use({
      root, binary, absentBinary: join(root, 'absent-native'), home, stateDir, cwd,
      invocations: () => readFileSync(log, 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line) as FixtureInvocation),
    });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}
