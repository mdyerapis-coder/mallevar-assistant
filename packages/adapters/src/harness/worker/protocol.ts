import type { ActionSpec, ContextPackage, HarnessDescriptor, OpenSessionInput, PermissionAnswer, ProjectPolicy, SessionRef, TurnInput, WorkerEvent } from '../../../../contracts/src/generated';
import { parseWireI64, parseWireU64 } from '../../../../contracts/src/wire';
import type { NativePermissionRequest } from '../native/host';

export type WorkerCode = 'worker_correlation' | 'worker_method' | 'worker_busy' | 'worker_permission_denied';
export class WorkerFault extends Error {
  constructor(readonly code: WorkerCode) { super(code); }
}
export type NativeMethod =
  | { type: 'hello' } | { type: 'describe' }
  | { type: 'open'; input: OpenSessionInput }
  | { type: 'turn'; session: SessionRef; input: TurnInput }
  | { type: 'answer_permission'; session: SessionRef; answer: PermissionAnswer }
  | { type: 'interrupt' | 'export_context' | 'close'; session: SessionRef };
export type NativeResult =
  | { type: 'hello'; protocol: 2 }
  | { type: 'describe'; descriptor: HarnessDescriptor }
  | { type: 'open'; session: SessionRef }
  | { type: 'turn' | 'answer_permission' | 'interrupt' | 'close' }
  | { type: 'export_context'; context: ContextPackage };
export type HostMethod =
  | { type: 'state'; input: OpenSessionInput; turn?: TurnInput }
  | { type: 'prepare_action'; request: NativePermissionRequest; input: OpenSessionInput }
  | { type: 'verify_answer'; answer: PermissionAnswer; request: NativePermissionRequest }
  | { type: 'record_native_event'; event: Record<string, unknown>; input: OpenSessionInput };
export type HostResult =
  | { type: 'state'; state: { policy_version: ProjectPolicy['version']; state_digest: string } }
  | { type: 'prepare_action'; action: ActionSpec }
  | { type: 'verify_answer'; verified: boolean }
  | { type: 'record_native_event' };
export type RequestFrame = { protocol: 2; kind: 'request'; id: string; run_id: string; method: unknown };
export type HostReplyFrame = { protocol: 2; kind: 'host_reply'; id: string; request_id: string; run_id: string; session_id: string; result?: unknown; error?: { code: WorkerCode } };
export type IncomingFrame = RequestFrame | HostReplyFrame;
export type ReplyFrame = { protocol: 2; kind: 'reply'; id: string; run_id: string } & (
  | { result: NativeResult; error?: never }
  | { error: { code: WorkerCode }; result?: never }
);
export type EventFrame = { protocol: 2; kind: 'event'; id: string; run_id: string; event: WorkerEvent };
export type HostRequestFrame = { protocol: 2; kind: 'host_request'; id: string; request_id: string; run_id: string; session_id: string; method: HostMethod };
export type OutgoingFrame = ReplyFrame | EventFrame | HostRequestFrame;
const codes: Record<WorkerCode, true> = { worker_correlation: true, worker_method: true, worker_busy: true, worker_permission_denied: true };
const harnesses: Record<SessionRef['harness'], true> = { omp: true, codex: true, opencode: true, claude: true, hermes: true, alphacode: true };

/** Closed objects are checked before any field can affect native dispatch. */
export function closedObject(value: unknown, required: readonly string[], optional: readonly string[] = []): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new WorkerFault('worker_method');
  const record = value as Record<string, unknown>;
  if (required.some(key => !Object.hasOwn(record, key)) || Object.keys(record).some(key => !required.includes(key) && !optional.includes(key))) throw new WorkerFault('worker_method');
  return record;
}
export function uuid(value: unknown): asserts value is string {
  if (typeof value !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value)) throw new WorkerFault('worker_correlation');
}
function text(value: unknown): asserts value is string {
  if (typeof value !== 'string') throw new WorkerFault('worker_method');
}
function digest(value: unknown): asserts value is string {
  if (typeof value !== 'string' || !/^[0-9a-f]{64}$/.test(value)) throw new WorkerFault('worker_method');
}
function strings(value: unknown): asserts value is string[] {
  if (!Array.isArray(value) || value.some(item => typeof item !== 'string')) throw new WorkerFault('worker_method');
}
function integer(value: unknown): asserts value is number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new WorkerFault('worker_method');
}
/** Native u64 fields travel as canonical decimal strings on this wire. */
function wireU64(value: unknown): void {
  try { parseWireU64(value); } catch { throw new WorkerFault('worker_method'); }
}
/** Native i64 fields travel as canonical decimal strings on this wire. */
function wireI64(value: unknown): void {
  try { parseWireI64(value); } catch { throw new WorkerFault('worker_method'); }
}
function route(value: unknown): void {
  const item = closedObject(value, ['connection_id', 'model_id'], ['harness']);
  text(item['connection_id']); text(item['model_id']);
  if (item['harness'] !== undefined && (typeof item['harness'] !== 'string' || !Object.hasOwn(harnesses, item['harness']))) throw new WorkerFault('worker_method');
}
export function sessionRef(value: unknown): SessionRef {
  const item = closedObject(value, ['harness', 'native_id', 'native_version', 'project_id', 'cwd', 'resolved_route']);
  if (typeof item['harness'] !== 'string' || !Object.hasOwn(harnesses, item['harness'])) throw new WorkerFault('worker_method');
  text(item['native_id']); text(item['native_version']); uuid(item['project_id']); text(item['cwd']); route(item['resolved_route']);
  if (!item['native_id']) throw new WorkerFault('worker_correlation');
  return value as SessionRef;
}
function policy(value: unknown): void {
  const item = closedObject(value, ['version', 'project_id', 'allowed_connection_ids', 'allowed_tools', 'read_roots', 'write_roots', 'network_destinations', 'local_only', 'max_workers', 'max_requests', 'spend_policy', 'allow_unknown_cost'], ['max_cost_microusd']);
  wireU64(item['version']); uuid(item['project_id']);
  for (const field of ['allowed_connection_ids', 'allowed_tools', 'read_roots', 'write_roots', 'network_destinations']) strings(item[field]);
  integer(item['max_workers']); integer(item['max_requests']);
  if (item['max_cost_microusd'] !== undefined) wireU64(item['max_cost_microusd']);
  if (typeof item['local_only'] !== 'boolean' || typeof item['allow_unknown_cost'] !== 'boolean' || (item['spend_policy'] !== 'free_only' && item['spend_policy'] !== 'paid_allowed')) throw new WorkerFault('worker_method');
}
export function contextPackage(value: unknown): ContextPackage {
  const item = closedObject(value, ['digest', 'brief', 'constraints', 'excerpts', 'artifacts', 'unresolved_questions', 'originating_sessions']);
  digest(item['digest']); text(item['brief']); strings(item['constraints']); strings(item['artifacts']); strings(item['unresolved_questions']);
  if (!Array.isArray(item['excerpts']) || !Array.isArray(item['originating_sessions'])) throw new WorkerFault('worker_method');
  for (const session of item['originating_sessions']) sessionRef(session);
  for (const excerpt of item['excerpts']) {
    const part = closedObject(excerpt, ['text', 'citation', 'project_id', 'disclosure']);
    text(part['text']); uuid(part['project_id']);
    const disclosure = closedObject(part['disclosure'], ['type'], ['connection_ids']);
    if (disclosure['type'] === 'allowed_connections') {
      closedObject(part['disclosure'], ['type', 'connection_ids']); strings(disclosure['connection_ids']);
    } else if (disclosure['type'] === 'local_only' || disclosure['type'] === 'explicit_public') closedObject(part['disclosure'], ['type']);
    else throw new WorkerFault('worker_method');
    const citation = closedObject(part['citation'], ['source_id', 'revision_digest', 'locator']);
    text(citation['source_id']); digest(citation['revision_digest']);
    const locator = closedObject(citation['locator'], [], ['page', 'section', 'line_start', 'line_end', 'sheet', 'cell_range', 'bounding_box', 'time_start_ms', 'time_end_ms']);
    for (const key of ['section', 'sheet', 'cell_range']) if (locator[key] !== undefined) text(locator[key]);
    for (const key of ['page', 'line_start', 'line_end']) if (locator[key] !== undefined) integer(locator[key]);
    for (const key of ['time_start_ms', 'time_end_ms']) if (locator[key] !== undefined) wireU64(locator[key]);
    if (locator['bounding_box'] !== undefined && (!Array.isArray(locator['bounding_box']) || locator['bounding_box'].length !== 4 || locator['bounding_box'].some(n => typeof n !== 'number' || !Number.isFinite(n)))) throw new WorkerFault('worker_method');
  }
  return value as ContextPackage;
}
export function openInput(value: unknown): OpenSessionInput {
  const item = closedObject(value, ['run_id', 'project_id', 'cwd', 'route', 'context', 'policy'], ['resume']);
  uuid(item['run_id']); uuid(item['project_id']); text(item['cwd']); route(item['route']); contextPackage(item['context']); policy(item['policy']);
  if (item['resume'] !== undefined) sessionRef(item['resume']);
  const input = value as OpenSessionInput;
  if (input.policy.project_id !== input.project_id) throw new WorkerFault('worker_correlation');
  return input;
}
export function turnInput(value: unknown): TurnInput {
  const item = closedObject(value, ['request_id', 'message_id', 'content', 'allowed_tools', 'budget']);
  uuid(item['request_id']); uuid(item['message_id']); policy(item['budget']);
  if (!Array.isArray(item['content']) || !Array.isArray(item['allowed_tools'])) throw new WorkerFault('worker_method');
  for (const part of item['content']) {
    const content = closedObject(part, ['type'], ['text', 'uri', 'mime_type', 'artifact_id']);
    switch (content['type']) {
      case 'text': closedObject(part, ['type', 'text']); text(content['text']); break;
      case 'audio': case 'image': closedObject(part, ['type', 'uri', 'mime_type']); text(content['uri']); text(content['mime_type']); break;
      case 'artifact': closedObject(part, ['type', 'artifact_id']); text(content['artifact_id']); break;
      default: throw new WorkerFault('worker_method');
    }
  }
  for (const tool of item['allowed_tools']) { const descriptor = closedObject(tool, ['name', 'description', 'input_schema']); text(descriptor['name']); text(descriptor['description']); }
  return value as TurnInput;
}
export function actionSpec(value: unknown): ActionSpec {
  const item = closedObject(value, ['id', 'run_id', 'project_id', 'kind', 'canonical_args', 'scope', 'policy_version', 'expected_state_digest'], ['destination']);
  uuid(item['id']); uuid(item['run_id']); uuid(item['project_id']); text(item['kind']); wireU64(item['policy_version']); digest(item['expected_state_digest']);
  if (item['destination'] !== undefined) text(item['destination']);
  return value as ActionSpec;
}
export function permissionAnswer(value: unknown): PermissionAnswer {
  const item = closedObject(value, ['native_request_id', 'action', 'decision'], ['grant']);
  text(item['native_request_id']); actionSpec(item['action']);
  if (item['decision'] !== 'allow' && item['decision'] !== 'deny') throw new WorkerFault('worker_method');
  if (item['grant'] !== undefined) {
    const grant = closedObject(item['grant'], ['id', 'action_id', 'action_digest', 'policy_version', 'expected_state_digest', 'expires_at'], ['revoked_at']);
    uuid(grant['id']); uuid(grant['action_id']); digest(grant['action_digest']); wireU64(grant['policy_version']); digest(grant['expected_state_digest']); wireI64(grant['expires_at']);
    if (grant['revoked_at'] !== undefined) wireI64(grant['revoked_at']);
  }
  return value as PermissionAnswer;
}
export function nativeMethod(value: unknown): NativeMethod {
  const item = closedObject(value, ['type'], ['input', 'session', 'answer']);
  switch (item['type']) {
    case 'hello': case 'describe': closedObject(value, ['type']); break;
    case 'open': closedObject(value, ['type', 'input']); openInput(item['input']); break;
    case 'turn': closedObject(value, ['type', 'session', 'input']); sessionRef(item['session']); turnInput(item['input']); break;
    case 'answer_permission': closedObject(value, ['type', 'session', 'answer']); sessionRef(item['session']); permissionAnswer(item['answer']); break;
    case 'interrupt': case 'export_context': case 'close': closedObject(value, ['type', 'session']); sessionRef(item['session']); break;
    default: throw new WorkerFault('worker_method');
  }
  return value as NativeMethod;
}
export function incomingFrame(value: unknown): IncomingFrame {
  const base = closedObject(value, ['protocol', 'kind', 'id', 'run_id'], ['method', 'request_id', 'session_id', 'result', 'error']);
  if (base['protocol'] !== 2) throw new WorkerFault('worker_correlation');
  uuid(base['id']); uuid(base['run_id']);
  if (base['kind'] === 'request') closedObject(value, ['protocol', 'kind', 'id', 'run_id', 'method']);
  else if (base['kind'] === 'host_reply') {
    closedObject(value, ['protocol', 'kind', 'id', 'request_id', 'run_id', 'session_id'], ['result', 'error']);
    uuid(base['request_id']); text(base['session_id']);
    if (Object.hasOwn(base, 'result') === Object.hasOwn(base, 'error')) throw new WorkerFault('worker_correlation');
    if (Object.hasOwn(base, 'error')) {
      const error = closedObject(base['error'], ['code']);
      if (typeof error['code'] !== 'string' || !Object.hasOwn(codes, error['code'])) throw new WorkerFault('worker_correlation');
    }
  } else throw new WorkerFault('worker_correlation');
  return value as IncomingFrame;
}
export function hostResult(value: unknown, type: HostMethod['type']): HostResult {
  const item = closedObject(value, ['type'], ['state', 'action', 'verified']);
  if (item['type'] !== type) throw new WorkerFault('worker_correlation');
  switch (type) {
    case 'state': {
      closedObject(value, ['type', 'state']);
      const state = closedObject(item['state'], ['policy_version', 'state_digest']);
      wireU64(state['policy_version']); digest(state['state_digest']); break;
    }
    case 'prepare_action': closedObject(value, ['type', 'action']); actionSpec(item['action']); break;
    case 'verify_answer': closedObject(value, ['type', 'verified']); if (typeof item['verified'] !== 'boolean') throw new WorkerFault('worker_correlation'); break;
    case 'record_native_event': closedObject(value, ['type']); break;
  }
  return value as HostResult;
}
/** Equality for closed JSON contracts, independent of object key insertion order. */
export function sameJson(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (typeof left !== 'object' || left === null || typeof right !== 'object' || right === null) return false;
  if (Array.isArray(left) || Array.isArray(right)) return Array.isArray(left) && Array.isArray(right) && left.length === right.length && left.every((value, index) => sameJson(value, right[index]));
  const a = left as Record<string, unknown>, b = right as Record<string, unknown>;
  const keys = Object.keys(a);
  return keys.length === Object.keys(b).length && keys.every(key => Object.hasOwn(b, key) && sameJson(a[key], b[key]));
}

export function harnessDescriptor(value: unknown): HarnessDescriptor {
  const item = closedObject(value, ['harness', 'version', 'transport', 'route_compatibility', 'approval', 'cancel', 'resume', 'child_observability', 'capabilities']);
  if (typeof item['harness'] !== 'string' || !Object.hasOwn(harnesses, item['harness'])) throw new WorkerFault('worker_method');
  text(item['version']); text(item['transport']); strings(item['route_compatibility']);
  const capabilityStates: Record<string, true> = { unknown: true, unavailable: true, unqualified: true, qualified: true };
  for (const key of ['approval', 'cancel', 'resume', 'child_observability']) {
    if (typeof item[key] !== 'string' || !Object.hasOwn(capabilityStates, item[key])) throw new WorkerFault('worker_method');
  }
  if (!Array.isArray(item['capabilities'])) throw new WorkerFault('worker_method');
  for (const observation of item['capabilities']) {
    const capability = closedObject(observation, ['name', 'state', 'implementation_version', 'observed_at'], ['evidence_digest']);
    text(capability['name']); text(capability['implementation_version']); wireI64(capability['observed_at']);
    if (typeof capability['state'] !== 'string' || !Object.hasOwn(capabilityStates, capability['state'])) throw new WorkerFault('worker_method');
    if (capability['evidence_digest'] !== undefined) digest(capability['evidence_digest']);
  }
  return value as HarnessDescriptor;
}

/** Native adapters emit a closed subset of CoreEvent; they cannot emit core
 * policy/state/judgment/artifact assertions on this execution channel. */
export function nativeEvent(value: unknown): WorkerEvent {
  const item = closedObject(value, ['turn_id', 'run_id', 'payload'], ['session_id', 'tool_id']);
  uuid(item['run_id']); text(item['turn_id']); text(item['session_id']);
  if (!item['turn_id'] || !item['session_id']) throw new WorkerFault('worker_correlation');
  if (item['tool_id'] !== undefined) text(item['tool_id']);
  const payload = closedObject(item['payload'], ['type'], ['message_id', 'delta', 'text', 'call', 'content', 'route', 'action', 'action_id', 'result', 'usage', 'code', 'message', 'details', 'status', 'stop_reason']);
  switch (payload['type']) {
    case 'turn_started': closedObject(payload, ['type', 'message_id']); uuid(payload['message_id']); break;
    case 'text_delta': case 'reasoning_delta': closedObject(payload, ['type', 'message_id', 'delta']); uuid(payload['message_id']); text(payload['delta']); break;
    case 'text_replace': closedObject(payload, ['type', 'message_id', 'text']); uuid(payload['message_id']); text(payload['text']); break;
    case 'provider_tool_call': {
      closedObject(payload, ['type', 'message_id', 'call']); uuid(payload['message_id']);
      const call = closedObject(payload['call'], ['id', 'name', 'arguments']); text(call['id']); text(call['name']); break;
    }
    case 'provider_message': closedObject(payload, ['type', 'message_id', 'content', 'route']); uuid(payload['message_id']); route(payload['route']); break;
    case 'tool_started': case 'approval_needed': closedObject(payload, ['type', 'action']); actionSpec(payload['action']); break;
    case 'tool_finished': closedObject(payload, ['type', 'action_id', 'result']); uuid(payload['action_id']); break;
    case 'usage_observed': {
      closedObject(payload, ['type', 'usage']);
      const usage = closedObject(payload['usage'], ['billing', 'source'], ['input_tokens', 'output_tokens', 'cost_microusd']);
      for (const key of ['input_tokens', 'output_tokens']) if (usage[key] !== undefined) integer(usage[key]);
      if (usage['cost_microusd'] !== undefined) wireU64(usage['cost_microusd']);
      if (usage['source'] !== 'provider' && usage['source'] !== 'unknown') throw new WorkerFault('worker_method');
      const billing = closedObject(usage['billing'], ['state'], ['cost_microusd']);
      if (billing['state'] !== 'unknown' && billing['state'] !== 'reported') throw new WorkerFault('worker_method');
      if (billing['cost_microusd'] !== undefined) wireU64(billing['cost_microusd']);
      break;
    }
    case 'error': closedObject(payload, ['type', 'code', 'message', 'details']); text(payload['code']); text(payload['message']); break;
    case 'turn_finished':
      closedObject(payload, ['type', 'message_id', 'status'], ['stop_reason']); uuid(payload['message_id']);
      if (payload['status'] !== 'succeeded' && payload['status'] !== 'cancelled' && payload['status'] !== 'failed') throw new WorkerFault('worker_method');
      if (payload['stop_reason'] !== undefined) text(payload['stop_reason']);
      break;
    default: throw new WorkerFault('worker_method');
  }
  return value as WorkerEvent;
}
