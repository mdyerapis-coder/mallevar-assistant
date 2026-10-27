import { describe, it, expect } from 'bun:test';

import { OmpAdapter } from '../src/harness/omp';
import { CodexAdapter } from '../src/harness/codex';
import { OpenCodeAdapter } from '../src/harness/opencode';
import {
  permissionReply,
  requireObservedStop,
  validateAlphaIdentity,
  type AlphaIdentity,
  type NativePermission,
} from '../src/native/alphacode';
import {
  checkTurnBudget,
  deriveActionSpec,
  policySandbox,
  type NativePermissionHost,
} from '../src/harness/native/host';
import { runProbe, spawnNativeProcess } from '../src/harness/native/process';
import { withNativeFixture } from './fixtures/native-process';
import type { HarnessKind, OpenSessionInput, SessionRef, TurnInput } from '../../contracts/src/generated';

/** A host that never silently allows: `prepareAction` is the core's decision. */
const host: NativePermissionHost = {
  async state() { return { policy_version: "1", state_digest: 'd'.repeat(64) }; },
  async prepareAction() { throw new Error('CoreDeniedUnknownPermission'); },
  async verifyAnswer() { return false; },
  async recordNativeEvent() { /* noop */ },
};

function input(harness: HarnessKind, overrides: Partial<OpenSessionInput> = {}): OpenSessionInput {
  return {
    run_id: 'run-1',
    project_id: 'proj-1',
    cwd: '/tmp',
    route: { connection_id: 'local', model_id: 'm', harness },
    context: { digest: 'd', brief: '', constraints: [], excerpts: [], artifacts: [], unresolved_questions: [], originating_sessions: [] },
    policy: {
      version: "1",
      project_id: 'proj-1',
      allowed_connection_ids: ['local'],
      allowed_tools: ['read'],
      read_roots: ['/tmp'],
      write_roots: ['/tmp'],
      network_destinations: [],
      local_only: true,
      max_workers: 1,
      max_requests: 4,
      max_cost_microusd: "0",
      spend_policy: 'free_only',
      allow_unknown_cost: false,
    },
    ...overrides,
  };
}

function turn(session: SessionRef, allowedTools: string[]): TurnInput {
  return {
    request_id: 'req-1',
    message_id: 'msg-1',
    content: [],
    allowed_tools: allowedTools.map(name => ({ name, description: '', input_schema: {} })),
    budget: input(session.harness as HarnessKind).policy,
  };
}

describe('native boundary — isolated discovery and owned process lifetime', () => {
  it('omp: executes an isolated version probe without fabricating qualification', async () => {
    await withNativeFixture('omp', 'version', async fixture => {
      const adapter = new OmpAdapter(host, { binary: fixture.binary, stateDir: fixture.stateDir });
      const descriptor = await adapter.describe();
      expect(descriptor.version).toBe('1.2.3');
      expect([descriptor.approval, descriptor.cancel, descriptor.resume, descriptor.child_observability]).toEqual([
        'unqualified', 'unqualified', 'unqualified', 'unqualified',
      ]);
      expect(fixture.invocations().map(call => ({ args: call.args, cwd: call.cwd }))).toEqual([
        { args: ['--version'], cwd: fixture.stateDir },
      ]);
    });
  });

  it('omp: rejects mismatched routes before native protocol dispatch', async () => {
    await withNativeFixture('omp', 'version', async fixture => {
      const adapter = new OmpAdapter(host, { binary: fixture.binary, stateDir: fixture.stateDir });
      const mismatched = input('omp', { cwd: fixture.cwd });
      mismatched.route.harness = 'codex';
      await expect(adapter.open(mismatched)).rejects.toThrow('SessionIdentityMismatch');
      expect(fixture.invocations().every(call => call.args.length === 1 && call.args[0] === '--version')).toBe(true);
    });
  });

  it('a failed executable probe preserves the actual exit and diagnostic', async () => {
    await withNativeFixture('hermes', 'failure', async fixture => {
      const result = await runProbe(fixture.binary, ['--version'], { cwd: fixture.cwd, env: { HOME: fixture.home } });
      expect(result.exit).toBe(23);
      expect(result.signal).toBeNull();
      expect(result.stdout).toBe('Hermes Agent v1.2.3\n');
      expect(result.stderr).toContain('fixture version probe failed');
      expect(fixture.invocations()).toHaveLength(1);
    });
  });

  it('a hanging probe is killed and reaped by its explicit deadline', async () => {
    await withNativeFixture('hermes', 'version', async fixture => {
      const result = await runProbe(fixture.binary, ['--hang'], { cwd: fixture.cwd, env: { HOME: fixture.home } }, 1000);
      expect(result.exit).toBeNull();
      expect(result.signal).toBe('SIGKILL');
      const calls = fixture.invocations();
      expect(calls).toHaveLength(1);
      expect(JSON.parse(result.stdout)).toEqual({ ready: true, pid: calls[0]!.pid });
      expect(() => process.kill(calls[0]!.pid, 0)).toThrow();
    });
  });

  it('native stop observes the owned process exit without stopping an unrelated process', async () => {
    await withNativeFixture('codex', 'version', async fixture => {
      const owned = spawnNativeProcess(fixture.binary, ['--hold'], { cwd: fixture.cwd, env: { HOME: fixture.home } });
      const unrelated = spawnNativeProcess(fixture.binary, ['--hold'], { cwd: fixture.cwd, env: { HOME: fixture.home } });
      try {
        expect(await owned.frames()[Symbol.asyncIterator]().next()).toEqual({ done: false, value: { ready: true, pid: owned.pid } });
        expect(await unrelated.frames()[Symbol.asyncIterator]().next()).toEqual({ done: false, value: { ready: true, pid: unrelated.pid } });
        await owned.kill(false);
        expect(await owned.exit).toEqual({ code: null, signal: 'SIGTERM' });
        expect(() => process.kill(owned.pid!, 0)).toThrow();
        expect(() => process.kill(unrelated.pid!, 0)).not.toThrow();
      } finally {
        await Promise.all([owned.kill(true), unrelated.kill(true)]);
        await Promise.all([owned.exit, unrelated.exit]);
      }
    });
  });

  it('alphacode: an unpatched/absent permission extension is refused, never simulated', async () => {
    // Identity and permission validation is not live AlphaCode qualification.
    const identity: AlphaIdentity = {
      buildDigest: 'a'.repeat(64),
      effectiveRoute: { connection_id: 'local', model_id: 'm' },
      cwd: '/tmp',
      allowedTools: [],
      profile: 'ironbark-managed',
      socket: '/run/ironbark/alphacode.sock',
    };

    // The pinned permission extension is mandatory: a handshake reporting no
    // active gate is refused outright rather than run unenforced.
    expect(() => validateAlphaIdentity({ permissionsVersion: 1, gateActive: false }, identity)).toThrow(/UnpatchedAlphaCode/);
    expect(() => validateAlphaIdentity({}, identity)).toThrow(/UnpatchedAlphaCode/);
    // A wrong build/route/tool set is an identity mismatch, not a silent reuse.
    expect(() => validateAlphaIdentity(
      { permissionsVersion: 1, gateActive: true, buildDigest: 'b'.repeat(64), effectiveRoute: identity.effectiveRoute, cwd: identity.cwd, allowedTools: identity.allowedTools, profile: identity.profile, socket: identity.socket },
      identity,
    )).toThrow(/SessionIdentityMismatch/);

    // A permission reply is bound to the exact request and action.
    const request: NativePermission = {
      request_id: 'req-1',
      action: { session_id: 's', tool_call_id: 'tc', resolved_name: 'write', canonical_input: {}, cwd: '/tmp', route: identity.effectiveRoute, policy_version: "1", state_digest: 'd'.repeat(64) },
    };
    expect(() => permissionReply(request, { ...request, request_id: 'other', allowed: true })).toThrow(/PermissionIdentityMismatch/);
    expect(permissionReply(request, { ...request, allowed: false }).allowed).toBe(false);

    // A cancel is only a stop receipt when the children are observed stopped.
    expect(() => requireObservedStop({ session_id: 's', cancelled: false, active_tools: 1, children: [{ terminated: false }] }, 's')).toThrow(/NativeChildrenNotObservedStopped/);
    expect(() => requireObservedStop({ session_id: 's', cancelled: true, active_tools: 0, children: [{ terminated: true }] }, 's')).not.toThrow();
  });
});

describe('native boundary — no fabricated sessions or events', () => {
  it('a turn without an open session fails instead of emitting events', async () => {
    await withNativeFixture('omp', 'version', async fixture => {
      const adapter = new OmpAdapter(host, { binary: fixture.absentBinary, stateDir: fixture.stateDir });
      const session: SessionRef = {
        harness: 'omp',
        native_id: 'never-opened',
        native_version: '0',
        project_id: 'proj-1',
        cwd: fixture.cwd,
        resolved_route: { connection_id: 'local', model_id: 'm' },
      };
      const collected: unknown[] = [];
      await expect((async () => {
        for await (const event of adapter.turn(session, turn(session, ['read']))) collected.push(event);
      })()).rejects.toThrow('SessionIdentityMismatch');
      expect(collected).toHaveLength(0);
      expect(fixture.invocations()).toEqual([]);
    });
  });

  it('an unavailable binary is reported, never substituted', async () => {
    await withNativeFixture('omp', 'version', async fixture => {
      const adapter = new OmpAdapter(host, { binary: fixture.absentBinary, stateDir: fixture.stateDir });
      const descriptor = await adapter.describe();
      expect(descriptor.version).toBe('');
      expect(descriptor.approval).toBe('unavailable');
      await expect(adapter.open(input('omp', { cwd: fixture.cwd }))).rejects.toThrow(/ompUnavailable/);
      expect(fixture.invocations()).toEqual([]);
    });
  });

  it('interrupt without a session fails rather than reporting a false stop', async () => {
    await withNativeFixture('codex', 'version', async fixture => {
      const adapter = new CodexAdapter(host, { binary: fixture.binary, home: fixture.home });
      const session: SessionRef = {
        harness: 'codex',
        native_id: 'never-opened',
        native_version: '0',
        project_id: 'proj-1',
        cwd: fixture.cwd,
        resolved_route: { connection_id: 'local', model_id: 'm' },
      };
      await expect(adapter.interrupt(session)).rejects.toThrow('SessionIdentityMismatch');
      expect(fixture.invocations()).toEqual([]);
    });
  });
});

describe('native boundary — authority comes from core state, not the model', () => {
  it('a permission action binds the session run, project and policy version', () => {
    const session = input('omp');
    const action = deriveActionSpec(session, 'fs.write', 'file_write', { path: '/tmp/x' });
    expect(action.run_id).toBe(session.run_id);
    expect(action.project_id).toBe(session.project_id);
    expect(action.policy_version).toBe(session.policy.version);
    expect(action.kind).toBe('file_write');
    expect(action.scope).toEqual({ harness: 'omp', cwd: session.cwd });
  });

  it('a turn whose budget differs from the bound session is refused', () => {
    const session = input('omp');
    const drifted: TurnInput = {
      request_id: 'req-2',
      message_id: 'msg-2',
      content: [],
      allowed_tools: [{ name: 'read', description: '', input_schema: {} }],
      budget: { ...session.policy, allowed_tools: ['read', 'shell'] },
    };
    expect(() => checkTurnBudget({ input: session }, drifted)).toThrow(/ManagedTurnPolicyMismatch/);
  });

  it('a turn with no allowed tool is refused (tools are never implied)', () => {
    const session = input('omp');
    const tooless: TurnInput = {
      request_id: 'req-3',
      message_id: 'msg-3',
      content: [],
      allowed_tools: [],
      budget: session.policy,
    };
    expect(() => checkTurnBudget({ input: session }, tooless)).toThrow(/ManagedTurnToolsRequired/);
  });

  it('the native sandbox posture is derived from the project policy', () => {
    const readOnly = policySandbox({ ...input('omp').policy, write_roots: [] });
    expect(readOnly.sandbox).toBe('read-only');
    expect(readOnly.approvalPolicy).toBe('on-request');
    const writable = policySandbox({ ...input('omp').policy, write_roots: ['/tmp'] });
    expect(writable.sandbox).toBe('workspace-write');
  });

  it('a resumed session with another project identity is refused before native dispatch', async () => {
    await withNativeFixture('opencode', 'version', async fixture => {
      const adapter = new OpenCodeAdapter(host, { binary: fixture.binary, stateDir: fixture.stateDir });
      const mismatched: SessionRef = {
        harness: 'opencode',
        native_id: 'other-project-session',
        native_version: '0',
        project_id: 'another-project',
        cwd: fixture.cwd,
        resolved_route: { connection_id: 'local', model_id: 'm' },
      };
      await expect(adapter.open(input('opencode', { cwd: fixture.cwd, resume: mismatched }))).rejects.toThrow('SessionIdentityMismatch');
      expect(fixture.invocations().every(call => call.args.length === 1 && call.args[0] === '--version')).toBe(true);
    });
  });
});
