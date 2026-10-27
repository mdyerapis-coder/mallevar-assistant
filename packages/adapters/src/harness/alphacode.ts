import type { HarnessAdapter } from './types';
import type { ActionSpec, ContextPackage, HarnessDescriptor, OpenSessionInput, PermissionAnswer, SessionRef, TurnInput, WorkerEvent } from '../../../contracts/src/generated';
import { createHash } from 'node:crypto';
import { AlphaNativeClient, canonical, object, permissionReply, type NativeAction, type NativePermission } from '../native/alphacode';

/** These callbacks cross the trusted Rust core boundary; never implement policy in this worker. */
export type AlphaPermissionHost = {
  state(input: OpenSessionInput, turn?: TurnInput): Promise<{ policy_version: string; state_digest: string }>;
  prepareAction(request: NativePermission, input: OpenSessionInput): Promise<ActionSpec>;
  verifyAnswer(answer: PermissionAnswer, request: NativePermission): Promise<boolean>;
  recordNativeEvent(event: Record<string, unknown>, input: OpenSessionInput): Promise<void>;
};
export class AlphaCodeAdapter implements HarnessAdapter {
  #session?: { input: OpenSessionInput; ref: SessionRef };
  #requests = new Map<string, { native: NativePermission; action: ActionSpec }>();
  #tools = new Map<string, ActionSpec>();
  constructor(readonly native: AlphaNativeClient, readonly host: AlphaPermissionHost) {}
  async describe(): Promise<HarnessDescriptor> {
    return { harness: 'alphacode', version: '1.0.79+ironbark-permissions-v1', transport: 'owned-stdio-acp', route_compatibility: ['explicit-managed-profile'], approval: 'unqualified', cancel: 'unqualified', resume: 'unqualified', child_observability: 'unqualified', capabilities: [] };
  }
  async open(input: OpenSessionInput): Promise<SessionRef> {
    if (this.#session) throw new Error('AlphaCodeAlreadyOpen');
    const expected = this.native.identity;
    if (input.route.harness && input.route.harness !== 'alphacode') throw new Error('SessionIdentityMismatch');
    if (input.cwd !== expected.cwd || canonical(input.route) !== canonical(expected.effectiveRoute) || canonical(input.policy.allowed_tools) !== canonical(expected.allowedTools)) throw new Error('SessionIdentityMismatch');
    if (input.resume && (input.resume.harness !== 'alphacode' || input.resume.project_id !== input.project_id || input.resume.cwd !== input.cwd || canonical(input.resume.resolved_route) !== canonical(input.route))) throw new Error('SessionIdentityMismatch');
    await this.native.initialize();
    const result = object(await this.native.request(input.resume ? 'session/resume' : 'session/new', { cwd: input.cwd, mcpServers: [], ...(input.resume ? { sessionId: input.resume.native_id } : {}) }));
    const id = input.resume?.native_id ?? result.sessionId;
    if (typeof id !== 'string' || !id) throw new Error('MissingNativeSessionIdentity');
    const state = await this.host.state(input);
    await this.native.request('_ironbark/bind', { sessionId: id, ...state });
    const ref: SessionRef = { harness: 'alphacode', native_id: id, native_version: '1.0.79+ironbark-permissions-v1', project_id: input.project_id, cwd: input.cwd, resolved_route: input.route };
    this.#session = { input, ref }; return ref;
  }
  #bound(ref: SessionRef) {
    if (!this.#session || canonical(this.#session.ref) !== canonical(ref)) throw new Error('SessionIdentityMismatch');
    return this.#session;
  }
  async *turn(session: SessionRef, input: TurnInput): AsyncIterable<WorkerEvent> {
    const bound = this.#bound(session);
    if (canonical(input.budget) !== canonical(bound.input.policy) || canonical(input.allowed_tools.map(t => t.name)) !== canonical(this.native.identity.allowedTools)) throw new Error('ManagedTurnPolicyMismatch');
    const state = await this.host.state(bound.input, input);
    await this.native.request('_ironbark/bind', { sessionId: session.native_id, ...state });
    const prompt = input.content.map(part => {
      if (part.type !== 'text') throw new Error('AlphaContentRequiresQualifiedNativeAttachment');
      return { type: 'text', text: part.text };
    });
    prompt.unshift({ type: 'text', text: `Ironbark context package (artifact IDs are references, not loaded contents):\n${JSON.stringify(bound.input.context)}` });
    let nativeTurnId = input.request_id; // Request correlation only until native start; never used as success evidence.
    const event = (payload: WorkerEvent['payload'], tool_id?: string): WorkerEvent => ({ run_id: bound.input.run_id, session_id: session.native_id, turn_id: nativeTurnId, tool_id, payload });
    try {
      for await (const frame of this.native.prompt(session.native_id, prompt)) {
        const params = object(frame.params);
        if (frame.method === '_ironbark/turn_started') {
          if (params.sessionId !== session.native_id || typeof params.nativeTurnId !== 'number' || !Number.isSafeInteger(params.nativeTurnId)) throw new Error('MissingNativeTurnIdentity');
          nativeTurnId = String(params.nativeTurnId);
          yield event({ type: 'turn_started', message_id: input.message_id });
        } else if (frame.method === '_ironbark/tool_started') {
          const raw = object(params.action);
          if (params.sessionId !== session.native_id || typeof raw.tool_call_id !== 'string') throw new Error('InvalidNativeToolStart');
          const action = this.#tools.get(raw.tool_call_id);
          if (!action || canonical(action.canonical_args) !== canonical(raw)) throw new Error('UncorrelatedNativeToolStart');
          yield event({ type: 'tool_started', action }, raw.tool_call_id);
        } else if (frame.method === '_alphacode/server_event') {
          if (params.sessionId !== session.native_id) throw new Error('CrossSessionNativeEvent');
          const raw = object(params.event);
          await this.host.recordNativeEvent(raw, bound.input);
          if (raw.type === 'tokens') {
            if (typeof raw.input !== 'number' || typeof raw.output !== 'number' || !Number.isInteger(raw.input) || !Number.isInteger(raw.output) || raw.input < 0 || raw.output < 0 || raw.input > 0xffff_ffff || raw.output > 0xffff_ffff) throw new Error('InvalidNativeUsage');
            yield event({ type: 'usage_observed', usage: { input_tokens: raw.input, output_tokens: raw.output, billing: { state: 'unknown' }, source: 'provider' } });
          }
        } else if (frame.method === 'session/request_permission') {
          const raw = object(object(params._ironbark).action);
          if (typeof frame.id !== 'string' || raw.session_id !== session.native_id || typeof raw.tool_call_id !== 'string' || typeof raw.resolved_name !== 'string' || raw.cwd !== bound.input.cwd || canonical(raw.route) !== canonical(bound.input.route) || raw.policy_version !== state.policy_version || raw.state_digest !== state.state_digest || !Object.hasOwn(raw, 'canonical_input') || !this.native.identity.allowedTools.includes(raw.resolved_name)) throw new Error('InvalidNativePermission');
          const action: NativeAction = { session_id: session.native_id, tool_call_id: raw.tool_call_id, resolved_name: raw.resolved_name, canonical_input: raw.canonical_input, cwd: bound.input.cwd, route: bound.input.route, policy_version: state.policy_version, state_digest: state.state_digest };
          const request = { request_id: frame.id, action };
          const coreAction = await this.host.prepareAction(request, bound.input);
          if (coreAction.run_id !== bound.input.run_id || coreAction.project_id !== bound.input.project_id || coreAction.policy_version !== state.policy_version || coreAction.expected_state_digest !== state.state_digest || canonical(coreAction.canonical_args) !== canonical(action)) throw new Error('CoreNativeActionIdentityMismatch');
          this.#requests.set(frame.id, { native: request, action: coreAction }); this.#tools.set(action.tool_call_id, coreAction);
          yield event({ type: 'approval_needed', action: coreAction }, action.tool_call_id);
        } else if (frame.method === 'session/update') {
          if (params.sessionId !== session.native_id) throw new Error('CrossSessionEvent');
          const update = object(params.update);
          if (update.sessionUpdate === 'agent_message_chunk' || update.sessionUpdate === '_ironbark/text_replace') {
            const content = object(update.content); if (content.type !== 'text' || typeof content.text !== 'string') throw new Error('InvalidNativeText');
            yield event(update.sessionUpdate === '_ironbark/text_replace' ? { type: 'text_replace', message_id: input.message_id, text: content.text } : { type: 'text_delta', message_id: input.message_id, delta: content.text });
          } else if (update.sessionUpdate === 'tool_call_update' && ['failed', 'completed'].includes(String(update.status))) {
            const id = update.toolCallId; if (typeof id !== 'string') throw new Error('MissingNativeToolIdentity');
            const action = this.#tools.get(id); if (!action) throw new Error('UncorrelatedNativeTool');
            yield event({ type: 'tool_finished', action_id: action.id, result: { status: update.status, native: update.rawOutput } }, id);
          }
        } else if (frame.method === '_ironbark/turn_finished') {
          const cancelled = params.stopReason === 'cancelled';
          if (cancelled) await this.native.interrupt(session.native_id);
          yield event({ type: 'turn_finished', message_id: input.message_id, status: cancelled ? 'cancelled' : 'verifying', stop_reason: String(params.stopReason) });
        } else if (frame.id !== undefined) {
          await this.native.reply(String(frame.id), { allowed: false }); throw new Error('UnknownNativePermissionMethod');
        }
      }
    } catch (error) {
      yield event({ type: 'error', code: 'alphacode_native_failure', message: error instanceof Error ? error.message : 'AlphaCode failure', details: null });
      yield event({ type: 'turn_finished', message_id: input.message_id, status: 'failed' });
      throw error;
    } finally { this.#requests.clear(); this.#tools.clear(); }
  }
  async answerPermission(answer: PermissionAnswer): Promise<void> {
    const request = this.#requests.get(answer.native_request_id);
    if (!request || canonical(request.action) !== canonical(answer.action)) throw new Error('UnknownOrChangedNativePermission');
    const allowed = answer.decision === 'allow' && await this.host.verifyAnswer(answer, request.native);
    this.#requests.delete(answer.native_request_id);
    await this.native.reply(answer.native_request_id, permissionReply(request.native, { ...request.native, allowed }));
  }
  async interrupt(session: SessionRef): Promise<void> { this.#bound(session); await this.native.interrupt(session.native_id); }
  async exportContext(session: SessionRef): Promise<ContextPackage> {
    this.#bound(session);
    const result = object(await this.native.request('_ironbark/history', { sessionId: session.native_id }));
    if (!Array.isArray(result.messages)) throw new Error('InvalidNativeHistory');
    const brief = result.messages.map(value => { const message = object(value); if (typeof message.role !== 'string' || typeof message.content !== 'string') throw new Error('InvalidNativeHistory'); return `${message.role}: ${message.content}`; }).join('\n').slice(-65_536);
    return { digest: createHash('sha256').update(brief).digest('hex'), brief, constraints: [], excerpts: [], artifacts: [], unresolved_questions: [], originating_sessions: [session] };
  }
  async close(session: SessionRef): Promise<void> {
    this.#bound(session);
    try { await this.native.interrupt(session.native_id); await this.native.request('session/close', { sessionId: session.native_id }); }
    finally { this.#requests.clear(); this.#tools.clear(); this.#session = undefined; await this.native.close(); }
  }
}
