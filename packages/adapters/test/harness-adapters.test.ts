import { describe, it, expect, beforeAll, afterAll } from 'bun:test';
import { CodexAdapter } from '../src/harness/codex';
import { OpenCodeAdapter } from '../src/harness/opencode';
import { ClaudeAdapter } from '../src/harness/claude';
import { HermesAdapter } from '../src/harness/hermes';
import type { NativePermissionHost } from '../src/harness/native/host';
import type { ActionSpec, SessionRef, HarnessKind, OpenSessionInput } from '../../contracts/src/generated';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { HarnessAdapter } from '../src/harness/types';
import { withNativeFixture, type NativeFixture } from './fixtures/native-process';

/** Minimal host for offline tests — records calls but never allows. */
const testHost: NativePermissionHost = {
  async state() { return { policy_version: "0", state_digest: 'test' }; },
  async prepareAction() { throw new Error('testHost.prepareAction not implemented'); },
  async verifyAnswer() { return true; },
  async recordNativeEvent() { /* noop */ },
};

/** Keep every unit launch inside its test-owned workspace. */
function testInput(harness: HarnessKind, cwd: string, resume?: SessionRef): OpenSessionInput {
  return {
    run_id: `run_${harness}_test`,
    project_id: `proj_${harness}_test`,
    cwd,
    route: { connection_id: 'test', model_id: 'auto', harness },
    context: { digest: 'd', brief: '', constraints: [], excerpts: [], artifacts: [], unresolved_questions: [], originating_sessions: [] },
    policy: { version: "1", project_id: `proj_${harness}_test`, allowed_connection_ids: [], allowed_tools: [], read_roots: [], write_roots: [], network_destinations: [], local_only: false, max_workers: 1, max_requests: 1, max_cost_microusd: "0", spend_policy: 'free_only', allow_unknown_cost: false },
    resume,
  };
}

type UnitHarness = {
  name: HarnessKind;
  create(host: NativePermissionHost, fixture: NativeFixture, binary?: string): HarnessAdapter;
  unavailable: RegExp;
  noSession: string;
};

const unitHarnesses: UnitHarness[] = [
  { name: 'codex', create: (host, f, binary = f.binary) => new CodexAdapter(host, { binary, home: f.home }), unavailable: /CodexUnavailable/, noSession: 'CodexNoOpenSession' },
  { name: 'opencode', create: (host, f, binary = f.binary) => new OpenCodeAdapter(host, { binary, stateDir: f.stateDir }), unavailable: /opencodeUnavailable/, noSession: 'AcpNoOpenSession' },
  { name: 'claude', create: (host, f, binary = f.binary) => new ClaudeAdapter(host, { binary, home: f.home }), unavailable: /ClaudeUnavailable/, noSession: 'ClaudeNoOpenSession' },
  { name: 'hermes', create: (host, f, binary = f.binary) => new HermesAdapter(host, { binary, stateDir: f.stateDir }), unavailable: /hermesUnavailable/, noSession: 'AcpNoOpenSession' },
];

describe('harness adapters — isolated executable discovery and denial', () => {
  for (const { name, create, unavailable, noSession } of unitHarnesses) {
    describe(name, () => {
      it('executes a version probe without qualifying native capabilities or creating a session', async () => {
        await withNativeFixture(name, 'version', async fixture => {
          const adapter = create(testHost, fixture);
          const descriptor = await adapter.describe();
          expect(descriptor.version).toBe('1.2.3');
          expect([descriptor.approval, descriptor.cancel, descriptor.resume, descriptor.child_observability]).toEqual([
            'unqualified', 'unqualified', 'unqualified', 'unqualified',
          ]);
          expect(fixture.invocations().map(call => call.args)).toEqual([['--version']]);
          await expect(adapter.answerPermission({ native_request_id: 'unknown', action: {} as ActionSpec, decision: 'allow' })).rejects.toThrow(noSession);
        });
      });

      for (const mode of ['failure', 'malformed-version'] as const) {
        it(`refuses session creation after a ${mode} probe`, async () => {
          await withNativeFixture(name, mode, async fixture => {
            const adapter = create(testHost, fixture);
            const descriptor = await adapter.describe();
            expect(descriptor.version).toBe('');
            expect(descriptor.approval).toBe('unavailable');
            await expect(adapter.open(testInput(name, fixture.cwd))).rejects.toThrow(unavailable);
            expect(fixture.invocations().map(call => call.args)).toEqual([['--version'], ['--version']]);
          });
        });
      }

      it('reports an absent owned path and refuses session creation without fallback', async () => {
        await withNativeFixture(name, 'version', async fixture => {
          const adapter = create(testHost, fixture, fixture.absentBinary);
          const descriptor = await adapter.describe();
          expect(descriptor.version).toBe('');
          expect(descriptor.approval).toBe('unavailable');
          await expect(adapter.open(testInput(name, fixture.cwd))).rejects.toThrow(unavailable);
          expect(fixture.invocations()).toEqual([]);
        });
      });

      it('rejects route mismatch before native protocol dispatch', async () => {
        await withNativeFixture(name, 'version', async fixture => {
          const adapter = create(testHost, fixture);
          const mismatched = testInput(name, fixture.cwd);
          mismatched.route.harness = name === 'codex' ? 'opencode' : 'codex';
          await expect(adapter.open(mismatched)).rejects.toThrow('SessionIdentityMismatch');
          // A cheap version probe is permitted; starting any protocol is not.
          expect(fixture.invocations().every(call => call.args.length === 1 && call.args[0] === '--version')).toBe(true);
        });
      });

      it('fails closed on permission answers without launching any process', async () => {
        await withNativeFixture(name, 'version', async fixture => {
          const adapter = create(testHost, fixture);
          await expect(adapter.answerPermission({ native_request_id: 'unknown', action: {} as ActionSpec, decision: 'allow' })).rejects.toThrow(noSession);
          expect(fixture.invocations()).toEqual([]);
        });
      });

      it('rejects malformed native initialization and reaps its actual process', async () => {
        await withNativeFixture(name, 'malformed-protocol', async fixture => {
          const adapter = create(testHost, fixture);
          await expect(adapter.open(testInput(name, fixture.cwd))).rejects.toThrow();
          const dispatched = fixture.invocations().filter(call => !call.args.includes('--version'));
          expect(dispatched).toHaveLength(1);
          expect(() => process.kill(dispatched[0]!.pid, 0)).toThrow();
          await expect(adapter.answerPermission({ native_request_id: 'unknown', action: {} as ActionSpec, decision: 'allow' })).rejects.toThrow(noSession);
        });
      });
    });
  }
});

// A live round-trip needs an authenticated provider/CLI session, which the plan
// treats as owner-gated live qualification (the same gate the live smokes
// document). Without the explicit opt-in below the provider-less engines
// (claude/hermes on a machine with no configured provider) would fail on
// startup rather than on the adapter's behaviour, so the live round-trip runs
// only when the owner enables it: IRONBARK_LIVE_HARNESS=1.
const LIVE_HARNESS = process.env['IRONBARK_LIVE_HARNESS'] === '1';
if (!LIVE_HARNESS) {
  console.log('skipping live harness round-trip: set IRONBARK_LIVE_HARNESS=1 to run it (owner-gated live qualification)');
}

describe.skipIf(!LIVE_HARNESS)('harness adapters — owner-gated real CLI round-trip (open/exportContext/close)', () => {
  // Deliberately separate from unitHarnesses: opt-in qualification must invoke
  // installed CLIs, never a fixture executable.
  const harnesses = [
    { name: 'codex', factory: (home: string) => new CodexAdapter(testHost, { binary: 'codex', home }) },
    { name: 'opencode', factory: (stateDir: string) => new OpenCodeAdapter(testHost, { binary: 'opencode', stateDir }) },
    { name: 'claude', factory: (home: string) => new ClaudeAdapter(testHost, { binary: 'claude', home }) },
    { name: 'hermes', factory: (stateDir: string) => new HermesAdapter(testHost, { binary: 'hermes', stateDir }) },
  ] as const;

  for (const { name, factory } of harnesses) {
    describe(name, () => {
      let adapter: HarnessAdapter;
      let sessionRef: SessionRef;
      let liveRoot: string;

      beforeAll(async () => {
        liveRoot = mkdtempSync(join(tmpdir(), `ironbark-${name}-live-`));
        adapter = factory(liveRoot);
        const d = await adapter.describe();
        if (d.version === '') {
          console.log(`${name}: binary not callable, skipping real round-trip`);
          return;
        }
        sessionRef = await adapter.open(testInput(name, liveRoot));
        expect(sessionRef.harness).toBe(name);
        expect(sessionRef.native_id.length).toBeGreaterThan(0);
      }, 120_000);

      afterAll(async () => {
        try {
          if (sessionRef && adapter) {
            await adapter.close(sessionRef);
          }
        } finally {
          if (liveRoot) rmSync(liveRoot, { recursive: true, force: true });
        }
      });


      it('exportContext_returns_valid_package', async () => {
        if (!sessionRef) return;
        const ctx = await adapter.exportContext(sessionRef);
        expect(ctx.digest).toBeDefined();
        expect(ctx.brief).toBeDefined();
        expect(ctx.originating_sessions).toContainEqual(sessionRef);
      });

      it('answerPermission_unknown_id_fails_closed', async () => {
        if (!sessionRef) return;
        await expect(adapter.answerPermission({ native_request_id: 'unknown', action: {} as ActionSpec, decision: 'allow' })).rejects.toThrow(/UnknownNativePermission|NoOpenSession/);
      });

      it('interrupt_without_active_turn_throws', async () => {
        if (!sessionRef) return;
        await expect(adapter.interrupt(sessionRef)).rejects.toThrow(/NoActiveTurn/);
      });
    });
  }
});