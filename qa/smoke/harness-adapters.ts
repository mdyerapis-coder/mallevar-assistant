import { CodexAdapter } from '../../packages/adapters/src/harness/codex';
import { OpenCodeAdapter } from '../../packages/adapters/src/harness/opencode';
import { ClaudeAdapter } from '../../packages/adapters/src/harness/claude';
import { HermesAdapter } from '../../packages/adapters/src/harness/hermes';
import type { NativePermissionHost } from '../../packages/adapters/src/harness/native/host';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const testHost: NativePermissionHost = {
  async state() { return { policy_version: "0", state_digest: 'smoke' }; },
  async prepareAction() { throw new Error('smoke.prepareAction not implemented'); },
  async verifyAnswer() { return true; },
  async recordNativeEvent() { /* noop */ },
};

const baseTemp = join(tmpdir(), 'ironbark-smoke');

async function runSmoke(adapter: { describe(): Promise<import('../../packages/contracts/src/generated').HarnessDescriptor> }, name: string): Promise<void> {
  const descriptor = await adapter.describe();
  console.log(`${name}:`, JSON.stringify(descriptor, null, 2));
  if (descriptor.harness !== name) throw new Error(`${name}: harness kind mismatch — got ${descriptor.harness}`);
  if (descriptor.capabilities.length === 0) throw new Error(`${name}: no capabilities reported`);
  const allUnknown = descriptor.capabilities.every((c: { state: string }) => c.state === 'unknown');
  if (allUnknown) throw new Error(`${name}: all capabilities are Unknown`);
  if (descriptor.version === '') {
    console.warn(`${name}: binary not callable — ${descriptor.capabilities[0]?.implementation_version}`);
  } else {
    if (descriptor.version.length === 0) throw new Error(`${name}: binary callable but version empty`);
  }
}

async function main(): Promise<void> {
  const adapters = [
    { name: 'codex', create: () => new CodexAdapter(testHost, { binary: 'codex', home: join(baseTemp, 'codex') }) },
    { name: 'opencode', create: () => new OpenCodeAdapter(testHost, { binary: 'opencode', stateDir: join(baseTemp, 'opencode') }) },
    { name: 'claude', create: () => new ClaudeAdapter(testHost, { binary: 'claude', home: join(baseTemp, 'claude') }) },
    { name: 'hermes', create: () => new HermesAdapter(testHost, { binary: 'hermes', stateDir: join(baseTemp, 'hermes') }) },
  ] as const;

  for (const { name, create } of adapters) {
    try {
      await runSmoke(create(), name);
      console.log(`✅ ${name} smoke passed`);
    } catch (error) {
      console.error(`❌ ${name} smoke FAILED:`, error instanceof Error ? error.message : String(error));
      process.exitCode = 1;
    }
  }
}

main();