import type { ActionSpec, ContextPackage, OpenSessionInput, PermissionAnswer, ProjectPolicy, TurnInput } from '../../../../contracts/src/generated';
import { createHash } from 'node:crypto';

/** Deterministic canonical encoding shared by all native adapters (stable key order). */
export function canonical(value: unknown): string {
  if (value === null || value === undefined) return 'null';
  if (typeof value === 'string') return JSON.stringify(value);
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
  if (typeof value === 'object') {
    const entries = Object.entries(value).sort(([a], [b]) => (a < b ? -1 : 1));
    return '{' + entries.map(([k, v]) => JSON.stringify(k) + ':' + canonical(v)).join(',') + '}';
  }
  return 'unknown';
}

export function object(value: unknown): Record<string, unknown> {
  if (typeof value === 'object' && value !== null) return value as Record<string, unknown>;
  throw new Error('NativeObjectExpected');
}

/** SHA-256 over a canonical value, as the contract's Digest type. */
export function digest(value: unknown): string {
  return createHash('sha256').update(canonical(value)).digest('hex');
}

/** Core-issued policy state for a managed native session. Never derive policy here. */
export type NativeSessionState = { policy_version: string; state_digest: string };

/** A native permission request surfaced to the core before any approval reply is sent. */
export type NativePermissionRequest = {
  request_id: string;
  session_id: string;
  name: string;
  tool_call_id: string;
  input: unknown;
  raw: unknown;
};

/** These callbacks cross the trusted Rust core boundary; never implement policy in this worker. */
export type NativePermissionHost = {
  state(input: OpenSessionInput, turn?: TurnInput): Promise<NativeSessionState>;
  prepareAction(request: NativePermissionRequest, input: OpenSessionInput): Promise<ActionSpec>;
  verifyAnswer(answer: PermissionAnswer, request: NativePermissionRequest): Promise<boolean>;
  recordNativeEvent(event: Record<string, unknown>, input: OpenSessionInput): Promise<void>;
};

/** Build the managed context prefix sent as the first part of a turn prompt. */
export function contextPrefix(context: ContextPackage): string {
  return `Ironbark context package (artifact IDs are references, not loaded contents):\n${canonical(context)}`;
}

/** The turn prompt parts a native adapter sends, given the bound context and the new content. */
export function buildPrompt(context: ContextPackage, content: TurnInput['content']): string[] {
  const parts = [contextPrefix(context)];
  for (const part of content) {
    if (part.type === 'text') parts.push(part.text);
  }
  return parts;
}

/** Derive a local, core-consistent ActionSpec for tool starts that carry no native approval gate. */
export function deriveActionSpec(input: OpenSessionInput, toolId: string, kind: string, args: unknown): ActionSpec {
  return {
    id: `act_${digest({ toolId, kind })}`,
    run_id: input.run_id,
    project_id: input.project_id,
    kind,
    canonical_args: canonical(args),
    scope: { harness: input.route.harness ?? 'native', cwd: input.cwd },
    policy_version: input.policy.version,
    expected_state_digest: digest(input.policy),
  };
}

/** Validate an open session's route/policy against the bound input before acting. */
export function checkTurnBudget(bound: { input: OpenSessionInput }, input: TurnInput): void {
  if (canonical(input.budget) !== canonical(bound.input.policy)) throw new Error('ManagedTurnPolicyMismatch');
  // The turn's own tool list is the allowlist handed to the native engine. An
  // empty list would leave the engine with no declared tools, so refuse instead
  // of letting the native default (or the model) decide what may run.
  if (input.allowed_tools.length === 0) throw new Error('ManagedTurnToolsRequired');
}

export type PolicySandbox = {
  approvalPolicy: 'never' | 'on-request' | 'untrusted';
  sandbox: string;
  networkAccess: boolean;
};

/** Map the core project policy onto a native sandbox/approval posture. */
export function policySandbox(policy: ProjectPolicy): PolicySandbox {
  const write = policy.write_roots.length > 0;
  const sandbox = write ? 'workspace-write' : 'read-only';
  return {
    approvalPolicy: 'on-request',
    sandbox,
    networkAccess: !policy.local_only && policy.network_destinations.length > 0,
  };
}
