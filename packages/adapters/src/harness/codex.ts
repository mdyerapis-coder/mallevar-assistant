import { NATIVE_STATE_ROOT, describeExit, runProbe, spawnNativeProcess, type NativeProcess, type SpawnOptions, workerEnv } from './native/process';
import type { HarnessAdapter } from './types';
import type { ActionSpec, CapabilityState, CapabilityObservation, ContextPackage, HarnessDescriptor, OpenSessionInput, PermissionAnswer, RunState, SessionRef, TurnInput, WorkerEvent } from '../../../contracts/src/generated';
import { formatWireI64 } from '../../../contracts/src/wire';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { canonical, checkTurnBudget, contextPrefix, deriveActionSpec, digest, policySandbox, type NativePermissionHost, type NativePermissionRequest } from './native/host';
import { JsonRpcClient, object, versionFromProbe, type RpcStreamItem } from './native/rpc';

type ApprovalRecord = { request: NativePermissionRequest; action: ActionSpec; resolve: (allowed: boolean) => void };

type CodexSession = {
  input: OpenSessionInput;
  ref: SessionRef;
  process: NativeProcess;
  client: JsonRpcClient;
  threadId: string;
  activeTurnId?: string;
  approvals: Map<string, ApprovalRecord>;
  closed: boolean;
};

const APPROVAL_METHODS: Record<string, string> = {
  'item/commandExecution/requestApproval': 'codex.execCommand',
  'item/fileChange/requestApproval': 'codex.applyPatch',
  'item/permissions/requestApproval': 'codex.permissions',
  'execCommandApproval': 'codex.execCommand',
  'applyPatchApproval': 'codex.applyPatch',
};

/** A native item reporting this status is a tool/item failure: the turn must never be reported successful. */
const FAILED_ITEM_STATUS = 'failed';
/** Bounded window after `turn/completed` in which a late item is still observed, surfaced and counted. */
const LATE_ITEM_WINDOW_MS = 500;
/** Bounded window in which `interrupt()` may observe the real native exit before it fails closed. */
const STOP_OBSERVE_MS = 15_000;

/** Failure reason for a native `item`, or undefined when the item carries no failure signal. */
function failedItemReason(item: Record<string, unknown>): string | undefined {
  if (item['status'] !== FAILED_ITEM_STATUS) return undefined;
  const type = typeof item['type'] === 'string' ? item['type'] : 'item';
  const id = typeof item['id'] === 'string' ? item['id'] : '';
  return `${type}${id ? ` ${id}` : ''} ${FAILED_ITEM_STATUS}`;
}

/** Genuine Codex app-server (JSON-RPC over stdio) adapter. */
export class CodexAdapter implements HarnessAdapter {
  #binary: string;
  #home: string;
  #version = '';
  #session?: CodexSession;

  constructor(readonly host: NativePermissionHost, options: { binary?: string; home?: string } = {}) {
    this.#binary = options.binary ?? 'codex';
    this.#home = options.home ?? `${NATIVE_STATE_ROOT}/codex-home`;
  }

  #spawnOptions(cwd: string): SpawnOptions {
    return { cwd, env: { ...workerEnv(), HOME: this.#home, CODEX_HOME: `${this.#home}/.codex` } };
  }

  #ensureHome(): void {
    mkdirSync(`${this.#home}/.codex`, { recursive: true });
    const config = `${this.#home}/.codex/config.toml`;
    if (!existsSync(config)) {
      writeFileSync(config, [
        '# Managed by Ironbark; approval policy and sandbox are pinned per thread via app-server params.',
        'model = "gpt-5.2-codex"',
        '',
      ].join('\n'));
    }
  }

  async #probeVersion(): Promise<{ version: string; reason: string }> {
    this.#ensureHome();
    const probe = await runProbe(this.#binary, ['--version'], { cwd: this.#home }, 10_000).catch(error => ({ exit: null, signal: null, stdout: '', stderr: String(error) }));
    const version = versionFromProbe(probe, [/codex-cli\s+([\d.]+)/i, /codex\s+v?(\d+\.\d+\.\d+[\w.-]*)/i]);
    if (probe.exit !== 0 || version === '') return { version: '', reason: `codex --version ${describeExit(probe.exit, probe.signal)}: ${probe.stderr.slice(0, 200)}` };
    return { version, reason: '' };
  }

  async describe(): Promise<HarnessDescriptor> {
    const { version, reason } = await this.#probeVersion();
    const state: CapabilityState = version === '' ? 'unavailable' : 'unqualified';
    const implementation = version === '' ? `unavailable:${reason.slice(0, 120)}` : `probed:codex-cli ${version}`;
    const observation = (name: string): CapabilityObservation => ({ name, state, implementation_version: implementation, observed_at: formatWireI64(BigInt(Date.now())) });
    return {
      harness: 'codex',
      version,
      transport: 'stdio-json-rpc',
      route_compatibility: ['native-stdio-codex-app-server'],
      approval: state,
      cancel: state,
      resume: state,
      child_observability: state,
      capabilities: [observation('approval'), observation('cancel'), observation('resume'), observation('child_observability')],
    };
  }

  async #openSession(input: OpenSessionInput): Promise<CodexSession> {
    if (this.#session && !this.#session.closed) throw new Error('CodexAlreadyOpen');
    if (input.route.harness && input.route.harness !== 'codex') throw new Error('SessionIdentityMismatch');
    if (input.resume && (input.resume.harness !== 'codex' || input.resume.project_id !== input.project_id || input.resume.cwd !== input.cwd || canonical(input.resume.resolved_route) !== canonical(input.route))) throw new Error('SessionIdentityMismatch');
    this.#ensureHome();
    const process = spawnNativeProcess(this.#binary, ['app-server', '--stdio', '--strict-config'], this.#spawnOptions(input.cwd));
    const client = new JsonRpcClient(process);
    try {
      await client.request('initialize', { clientInfo: { name: 'ironbark', version: '0.1' } }, 20_000);
      const posture = policySandbox(input.policy);
      const model = input.route.model_id === 'auto' ? undefined : input.route.model_id;
      const params: Record<string, unknown> = { cwd: input.cwd, approvalPolicy: 'on-request', sandbox: posture.sandbox };
      if (model) params['model'] = model;
      if (input.resume) params['threadId'] = input.resume.native_id;
      const result = object(await client.request(input.resume ? 'thread/resume' : 'thread/start', params, 30_000));
      const thread = object(result['thread'] ?? result);
      const threadId = typeof thread['id'] === 'string' ? thread['id'] : '';
      if (!threadId) throw new Error('MissingNativeThreadIdentity');
      const cwdValue = typeof thread['cwd'] === 'string' ? thread['cwd'] : '';
      if (cwdValue && cwdValue !== input.cwd) throw new Error('SessionIdentityMismatch');
      const ref: SessionRef = {
        harness: 'codex',
        native_id: threadId,
        native_version: this.#version || 'codex-app-server',
        project_id: input.project_id,
        cwd: input.cwd,
        resolved_route: input.route,
      };
      const session: CodexSession = { input, ref, process, client, threadId, approvals: new Map(), closed: false };
      this.#session = session;
      return session;
    } catch (error) {
      await client.close();
      throw error instanceof Error ? error : new Error('CodexOpenFailed');
    }
  }

  async open(input: OpenSessionInput): Promise<SessionRef> {
    const { version, reason } = await this.#probeVersion();
    if (version === '') throw new Error(`CodexUnavailable: ${reason.slice(0, 160)}`);
    this.#version = version;
    const session = await this.#openSession(input);
    await this.host.recordNativeEvent({ event: 'session_bound', harness: 'codex', native_id: session.ref.native_id, native_version: version }, input).catch(() => undefined);
    return session.ref;
  }

  #bound(ref: SessionRef): CodexSession {
    if (!this.#session || this.#session.closed || this.#session.ref.native_id !== ref.native_id || canonical(this.#session.ref.resolved_route) !== canonical(ref.resolved_route)) throw new Error('SessionIdentityMismatch');
    return this.#session;
  }

  #workerEvent(session: CodexSession, turnId: string, payload: WorkerEvent['payload'], toolId?: string): WorkerEvent {
    return { run_id: session.input.run_id, session_id: session.ref.native_id, turn_id: turnId, tool_id: toolId, payload };
  }

  #itemEvents(session: CodexSession, turnId: string, item: Record<string, unknown>): Array<WorkerEvent> {
    const events: Array<WorkerEvent> = [];
    const id = typeof item['id'] === 'string' ? item['id'] : '';
    const type = typeof item['type'] === 'string' ? item['type'] : '';
    const status = typeof item['status'] === 'string' ? item['status'] : '';
    const toolId = id || undefined;
    if (type === 'subAgentActivity') {
      const childId = typeof item['agentThreadId'] === 'string' ? item['agentThreadId'] : id;
      events.push(this.#workerEvent(session, turnId, { type: 'state_changed', entity_id: childId, state: status || 'active' }, childId || undefined));
      return events;
    }
    const toolKinds: Record<string, string> = { commandExecution: 'codex.execCommand', fileChange: 'codex.applyPatch', mcpToolCall: 'codex.mcpToolCall', dynamicToolCall: 'codex.dynamicToolCall' };
    const kind = toolKinds[type];
    if (kind) {
      if (status === 'inProgress' || status === 'pending') {
        const action = deriveActionSpec(session.input, id, kind, { command: item['command'], changes: item['changes'], tool: item['tool'], server: item['server'] });
        events.push(this.#workerEvent(session, turnId, { type: 'tool_started', action }, toolId));
      } else if (status === 'completed' || status === 'failed' || status === 'declined') {
        const outcome: Record<string, unknown> = { status, exitCode: item['exitCode'] ?? null, durationMs: item['durationMs'] ?? null };
        if (type === 'commandExecution') outcome.output = String(item['aggregatedOutput'] ?? '').slice(-4_096);
        if (type === 'mcpToolCall') { outcome.result = item['result']; outcome.error = item['error'] ?? null; }
        events.push(this.#workerEvent(session, turnId, { type: 'tool_finished', action_id: `act_${digest({ toolId: id, kind })}`, result: outcome }, toolId));
      }
    }
    return events;
  }

  #approvalReply(method: string, allowed: boolean): unknown {
    if (method === 'item/commandExecution/requestApproval') return { decision: allowed ? 'accept' : 'decline' };
    if (method === 'item/fileChange/requestApproval') return { decision: allowed ? 'accept' : 'decline' };
    if (method === 'applyPatchApproval' || method === 'execCommandApproval') return { decision: allowed ? 'approved' : 'denied' };
    if (method === 'item/permissions/requestApproval') return { permissions: {} };
    return { denied: true };
  }

  #handleServerRequest(session: CodexSession, item: Extract<RpcStreamItem, { kind: 'server_request' }>, turnId: string, push: (event: WorkerEvent) => void): void {
    const params = object(item.params);
    const itemId = typeof params['itemId'] === 'string' ? params['itemId'] : typeof params['callId'] === 'string' ? params['callId'] : String(item.id);
    const kind = APPROVAL_METHODS[item.method];
    if (!kind) {
      void session.client.reply(item.id, { denied: true }).catch(() => undefined);
      void this.host.recordNativeEvent({ event: 'server_request_denied', method: item.method, item_id: itemId, thread_id: session.threadId }, session.input).catch(() => undefined);
      return;
    }
    const request: NativePermissionRequest = {
      request_id: String(item.id),
      session_id: session.ref.native_id,
      name: kind,
      tool_call_id: itemId,
      input: params,
      raw: params,
    };
    void (async () => {
      let action: ActionSpec;
      try {
        action = await this.host.prepareAction(request, session.input);
      } catch (error) {
        await session.client.reply(item.id, this.#approvalReply(item.method, false)).catch(() => undefined);
        await this.host.recordNativeEvent({ event: 'approval_failed_closed', method: item.method, item_id: itemId, reason: error instanceof Error ? error.message : String(error) }, session.input).catch(() => undefined);
        return;
      }
      const { promise, resolve } = Promise.withResolvers<boolean>();
      session.approvals.set(request.request_id, { request, action, resolve });
      push(this.#workerEvent(session, turnId, { type: 'approval_needed', action }, itemId));
      let allowed = false;
      try {
        allowed = await promise;
      } finally {
        session.approvals.delete(request.request_id);
        await session.client.reply(item.id, this.#approvalReply(item.method, allowed)).catch(() => undefined);
        await this.host.recordNativeEvent({ event: 'approval_answered', method: item.method, item_id: itemId, allowed }, session.input).catch(() => undefined);
      }
    })();
  }

  async *turn(session: SessionRef, input: TurnInput): AsyncIterable<WorkerEvent> {
    const bound = this.#bound(session);
    if (bound.activeTurnId) throw new Error('CodexTurnAlreadyActive');
    checkTurnBudget(bound, input);
    await this.host.state(bound.input, input);
    const parts = input.content.filter(part => part.type === 'text');
    if (parts.length === 0) throw new Error('CodexTurnRequiresTextContent');
    const text = [contextPrefix(bound.input.context), ...parts.map(part => (part.type === 'text' ? part.text : ''))].join('\n\n');
    const out: Array<WorkerEvent> = [];
    const push = (event: WorkerEvent): void => { out.push(event); };
    let sawTurn = false;
    let failureReason: string | undefined;
    let completion: { id: string; failed: boolean; cancelled: boolean; reason: string } | undefined;
    let lateDeadline = 0;
    try {
      await bound.client.request('turn/start', {
        threadId: bound.threadId,
        input: [{ type: 'text', text }],
        clientUserMessageId: input.message_id,
        approvalPolicy: 'on-request',
        cwd: bound.input.cwd,
      }, 60_000);
      while (true) {
        let item: RpcStreamItem;
        if (completion === undefined) {
          item = await bound.client.queue.take(120_000);
        } else {
          // `turn/completed` is not the end of the observable stream: a failed item
          // can still arrive after it. Keep reading inside a bounded window so such
          // an item is surfaced (and counted) instead of being swallowed.
          const remaining = lateDeadline - Date.now();
          if (remaining <= 0) break;
          try {
            item = await bound.client.queue.take(remaining);
          } catch {
            break;
          }
        }
        if (item.kind === 'server_request') {
          this.#handleServerRequest(bound, item, input.message_id, push);
          continue;
        }
        const params = object(item.params);
        const threadId = typeof params['threadId'] === 'string' ? params['threadId'] : '';
        if (threadId && threadId !== bound.threadId) continue;
        const turnId = typeof params['turnId'] === 'string' ? params['turnId'] : input.message_id;
        if (item.method === 'turn/started') {
          sawTurn = true;
          bound.activeTurnId = turnId;
          out.push(this.#workerEvent(bound, turnId, { type: 'turn_started', message_id: input.message_id }));
          continue;
        }
        if (item.method === 'item/agentMessage/delta') {
          const delta = typeof params['delta'] === 'string' ? params['delta'] : '';
          if (delta) out.push(this.#workerEvent(bound, turnId, { type: 'text_delta', message_id: input.message_id, delta }));
          continue;
        }
        if (item.method === 'item/reasoning/textDelta') {
          const delta = typeof params['delta'] === 'string' ? params['delta'] : '';
          if (delta) out.push(this.#workerEvent(bound, turnId, { type: 'reasoning_delta', message_id: input.message_id, delta }));
          continue;
        }
        if (item.method === 'item/started' || item.method === 'item/completed') {
          const itemObject = object(params['item'] ?? {});
          const failure = failedItemReason(itemObject);
          if (failure !== undefined) failureReason ??= failure;
          for (const event of this.#itemEvents(bound, turnId, itemObject)) out.push(event);
          continue;
        }
        if (item.method === 'error') {
          const error = object(params['error'] ?? {});
          const message = String(error['message'] ?? 'codex turn error');
          const willRetry = params['willRetry'] === true;
          out.push(this.#workerEvent(bound, turnId, { type: 'provider_message', message_id: input.message_id, content: { kind: 'codex_error', message, willRetry }, route: bound.input.route }));
          if (!willRetry) break;
          continue;
        }
        if (item.method === 'turn/completed') {
          const turnObject = object(params['turn'] ?? {});
          const completedId = typeof turnObject['id'] === 'string' ? String(turnObject['id']) : turnId;
          const turnStatus = typeof turnObject['status'] === 'string' ? turnObject['status'] : '';
          const error = turnObject['error'];
          const errorObject = typeof error === 'object' && error !== null ? object(error) : undefined;
          const failed = errorObject !== undefined || turnStatus === 'failed';
          const reason = errorObject !== undefined
            ? String(errorObject['message'] ?? 'codex turn failed')
            : failed ? 'codex turn failed' : turnStatus === 'interrupted' ? 'interrupted' : 'completed';
          completion = { id: completedId, failed, cancelled: turnStatus === 'interrupted', reason };
          lateDeadline = Date.now() + LATE_ITEM_WINDOW_MS;
          continue;
        }
        if (item.method === 'thread/closed') break;
      }
      if ((sawTurn || completion !== undefined) && !out.some(event => event.payload.type === 'turn_finished')) {
        if (completion === undefined) {
          out.push(this.#workerEvent(bound, input.message_id, { type: 'turn_finished', message_id: input.message_id, status: 'failed', stop_reason: 'native_stream_closed' }));
        } else {
          // The completion frame and every failure observed in the turn stream both
          // decide the status: a turn carrying a failed tool is never a success.
          const failed = completion.failed || failureReason !== undefined;
          const status: RunState = failed ? 'failed' : completion.cancelled ? 'cancelled' : 'succeeded';
          const stopReason = (completion.failed ? completion.reason : failureReason ?? completion.reason).slice(0, 200);
          out.push(this.#workerEvent(bound, completion.id, { type: 'turn_finished', message_id: input.message_id, status, stop_reason: stopReason }));
        }
      }
      // The turn stays bound until the late-item window has closed: a second turn
      // must never be allowed to start while this turn still owns the frame stream.
      bound.activeTurnId = undefined;
      for (const event of out) yield event;
    } catch (error) {
      bound.activeTurnId = undefined;
      const message = error instanceof Error ? error.message : String(error);
      yield this.#workerEvent(bound, input.message_id, { type: 'turn_finished', message_id: input.message_id, status: 'failed', stop_reason: message.slice(0, 200) });
    }
  }

  async answerPermission(answer: PermissionAnswer): Promise<void> {
    const session = this.#session;
    if (!session || session.closed) throw new Error('CodexNoOpenSession');
    const record = session.approvals.get(answer.native_request_id);
    if (!record) throw new Error('UnknownNativePermission');
    const verified = await this.host.verifyAnswer(answer, record.request);
    if (!verified) throw new Error('PermissionAnswerRejected');
    record.resolve(answer.decision === 'allow');
  }

  async interrupt(session: SessionRef): Promise<void> {
    const bound = this.#bound(session);
    if (!bound.activeTurnId) throw new Error('CodexNoActiveTurn');
    await bound.client.request('turn/interrupt', { threadId: bound.threadId, turnId: bound.activeTurnId }, 15_000);
    bound.activeTurnId = undefined;
    // The interrupt acknowledgement is not a stop: the receipt is only produced
    // once the native process has been observed exiting. If it never does inside
    // the bounded window this fails closed instead of reporting a stop.
    const { promise: window, resolve: windowElapsed } = Promise.withResolvers<false>();
    const timer = setTimeout(() => windowElapsed(false), STOP_OBSERVE_MS);
    timer.unref?.();
    const exited = await Promise.race([bound.process.exit.then(() => true), window]);
    clearTimeout(timer);
    if (!exited) throw new Error('CodexStopNotObserved');
  }

  async exportContext(session: SessionRef): Promise<ContextPackage> {
    const bound = this.#bound(session);
    let thread: Record<string, unknown>;
    try {
      const result = object(await bound.client.request('thread/read', { threadId: bound.threadId, includeTurns: true }, 30_000));
      thread = object(result['thread'] ?? {});
    } catch (error) {
      // Fallback: read without turns if the full read fails (e.g. list_turns unsupported)
      const result = object(await bound.client.request('thread/read', { threadId: bound.threadId, includeTurns: false }, 10_000));
      thread = object(result['thread'] ?? result);
    }
    const brief: string[] = [];
    const walkItem = (item: Record<string, unknown>): void => {
      const type = typeof item['type'] === 'string' ? item['type'] : '';
      if (type === 'userMessage') {
        const content = Array.isArray(item['content']) ? item['content'] : [];
        const text = content.map(part => (typeof part === 'object' && part !== null && typeof part['text'] === 'string' ? part['text'] : '')).join(' ').trim();
        if (text) brief.push(`user: ${text.slice(0, 4_000)}`);
      } else if (type === 'agentMessage' && typeof item['text'] === 'string' && item['text']) {
        brief.push(`assistant: ${item['text'].slice(0, 4_000)}`);
      } else if (type === 'commandExecution' && typeof item['command'] === 'string') {
        brief.push(`tool commandExecution: ${item['command'].slice(0, 2_000)} (exit=${String(item['exitCode'] ?? 'n/a')})`);
      } else if (type === 'fileChange' && typeof item['status'] === 'string') {
        brief.push(`tool fileChange: status=${item['status']}`);
      }
    };
    const turns = Array.isArray(thread['turns']) ? thread['turns'] : [];
    for (const turn of turns) {
      const turnObject = object(turn);
      const items = Array.isArray(turnObject['items']) ? turnObject['items'] : [];
      for (const item of items) walkItem(object(item));
    }
    if (turns.length === 0) walkItem(thread);
    const text = brief.join('\n').slice(0, 48_000);
    const posture = policySandbox(bound.input.policy);
    return {
      digest: digest(text),
      brief: text || `codex thread ${bound.threadId} (no turns)`,
      constraints: [`approval_policy=${posture.approvalPolicy}`, `sandbox=${posture.sandbox}`],
      excerpts: [],
      artifacts: [],
      unresolved_questions: [],
      originating_sessions: [bound.ref],
    };
  }

  async close(session: SessionRef): Promise<void> {
    const bound = this.#bound(session);
    if (bound.closed) return;
    bound.closed = true;
    for (const record of bound.approvals.values()) record.resolve(false);
    bound.approvals.clear();
    bound.activeTurnId = undefined;
    await bound.client.close();
    const exit = await bound.process.exit;
    await this.host.recordNativeEvent({ event: 'session_closed', harness: 'codex', native_id: session.native_id, exit: describeExit(exit.code, exit.signal) }, bound.input).catch(() => undefined);
  }
}