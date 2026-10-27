import { mkdirSync } from 'node:fs';
import { mkdir, open, readFile } from 'node:fs/promises';
import { dirname, isAbsolute, resolve, sep } from 'node:path';
import type { ActionSpec, CapabilityState, CapabilityObservation, ContextPackage, HarnessDescriptor, HarnessKind, OpenSessionInput, PermissionAnswer, SessionRef, TurnInput, WorkerEvent } from '../../../contracts/src/generated';
import { formatWireI64 } from '../../../contracts/src/wire';
import { canonical, checkTurnBudget, contextPrefix, deriveActionSpec, digest, type NativePermissionHost, type NativePermissionRequest } from './native/host';
import { NATIVE_STATE_ROOT, describeExit, runProbe, spawnNativeProcess, type NativeProcess, type SpawnOptions, workerEnv } from './native/process';
import { JsonRpcClient, object, versionFromProbe, type RpcStreamItem } from './native/rpc';
import type { HarnessAdapter } from './types';

const ACP_PROTOCOL_VERSION = 1;
const ACP_TURN_WINDOW_MS = 3_000_000;
const ACP_QUEUE_TAKE_MS = 120_000;
/**
 * ACP client file access (`fs/write_text_file`/`fs/read_text_file`) is served
 * only for the live native-harness smoke. OMP's edit tool routes writes through
 * the client, so without this the adapter advertises the capability and then
 * answers `{denied:true}`, which OMP treats as success while nothing is written.
 * Kept off the portable release path.
 */
const LIVE_FS_CLIENT = process.env.IRONBARK_LIVE_HARNESS === '1';
/** A client-requested path resolved inside the session worktree; any escape is rejected. */
function workspacePath(worktree: string, requested: string): string {
  if (!requested) throw new Error('fs_request_path_required');
  const root = resolve(worktree);
  const target = isAbsolute(requested) ? resolve(requested) : resolve(root, requested);
  if (target !== root && !target.startsWith(`${root}${sep}`)) throw new Error(`fs_request_outside_worktree:${target}`);
  return target;
}

type ApprovalRecord = { request: NativePermissionRequest; action: unknown; resolve: (result: { allowed: boolean; optionId?: string }) => void };
type AcpTurnState = {
  message_id: string;
  stop: { promise: Promise<{ ok: boolean; stop_reason?: string; message?: string }>; resolve: (value: { ok: boolean; stop_reason?: string; message?: string }) => void };
};

type AcpConfig = {
  harness: HarnessKind;
  binary: string;
  spawnArgs: string[];
  versionPatterns: RegExp[];
  versionArgs: string[];
  env: (stateDir: string) => Record<string, string>;
  stateDirName: string;
};

type AcpSession = {
  input: OpenSessionInput;
  ref: SessionRef;
  identity: string;
  process: NativeProcess;
  client: JsonRpcClient;
  sessionId: string;
  agentName: string;
  availableModels: Array<string>;
  approvals: Map<string, ApprovalRecord>;
  activeTurn?: AcpTurnState;
  closed: boolean;
};

/** Shared Agent Client Protocol (JSON-RPC over stdio) adapter for OpenCode and Hermes.
 *  Native permission requests are bridged to the core; unrecognised requests are denied (fail closed). */
export class AcpAdapterBase implements HarnessAdapter {
  #config: AcpConfig;
  #stateDir: string;
  #session?: AcpSession;

  constructor(readonly host: NativePermissionHost, config: AcpConfig, options: { stateDir?: string } = {}) {
    this.#config = config;
    this.#stateDir = options.stateDir ?? `${NATIVE_STATE_ROOT}/${config.stateDirName}`;
  }

  #spawnOptions(cwd: string): SpawnOptions {
    return { cwd, env: { ...workerEnv(), ...this.#config.env(this.#stateDir) } };
  }

  async #probeVersion(): Promise<{ version: string; reason: string }> {
    mkdirSync(this.#stateDir, { recursive: true });
    const probe = await runProbe(this.#config.binary, this.#config.versionArgs, { cwd: this.#stateDir }, 15_000).catch(error => ({ exit: null, signal: null, stdout: '', stderr: String(error) }));
    const version = versionFromProbe(probe, this.#config.versionPatterns);
    if (probe.exit !== 0 || version === '') return { version: '', reason: `${this.#config.binary} ${this.#config.versionArgs.join(' ')} ${describeExit(probe.exit, probe.signal)}: ${probe.stderr.slice(0, 200)}` };
    return { version, reason: '' };
  }

  async describe(): Promise<HarnessDescriptor> {
    const { version, reason } = await this.#probeVersion();
    const state: CapabilityState = version === '' ? 'unavailable' : 'unqualified';
    const implementation = version === '' ? `unavailable:${reason.slice(0, 120)}` : `probed:${this.#config.binary} ${version}`;
    const observation = (name: string): CapabilityObservation => ({ name, state, implementation_version: implementation, observed_at: formatWireI64(BigInt(Date.now())) });
    return {
      harness: this.#config.harness,
      version,
      transport: 'stdio-acp',
      route_compatibility: ['native-stdio-acp'],
      approval: state,
      cancel: state,
      resume: state,
      child_observability: state,
      capabilities: [observation('approval'), observation('cancel'), observation('resume'), observation('child_observability')],
    };
  }

  async #openSession(input: OpenSessionInput): Promise<AcpSession> {
    if (this.#session && !this.#session.closed) throw new Error('AcpAlreadyOpen');
    if (input.route.harness && input.route.harness !== this.#config.harness) throw new Error('SessionIdentityMismatch');
    if (input.resume && (input.resume.harness !== this.#config.harness || input.resume.project_id !== input.project_id || input.resume.cwd !== input.cwd || canonical(input.resume.resolved_route) !== canonical(input.route))) throw new Error('SessionIdentityMismatch');
    const process = spawnNativeProcess(this.#config.binary, this.#config.spawnArgs, this.#spawnOptions(input.cwd));
    const client = new JsonRpcClient(process);
    try {
      const initRaw = await client.request('initialize', {
        protocolVersion: ACP_PROTOCOL_VERSION,
        clientCapabilities: { fs: { readTextFile: true, writeTextFile: true } },
      }, 60_000);
      const init = object(initRaw);
      if (typeof init['protocolVersion'] !== 'number' || init['protocolVersion'] !== ACP_PROTOCOL_VERSION) throw new Error('AcpProtocolVersionMismatch');
      const agentInfo = object(init['agentInfo'] ?? {});
      const agentName = typeof agentInfo['name'] === 'string' ? agentInfo['name'] : this.#config.harness;
      const params: Record<string, unknown> = { cwd: input.cwd, mcpServers: [] };
      if (input.resume) params['sessionId'] = input.resume.native_id;
      const resultRaw = await client.request(input.resume ? 'session/resume' : 'session/new', params, 120_000);
      const result = object(resultRaw);
      const sessionId = typeof result['sessionId'] === 'string' ? result['sessionId'] : '';
      if (!sessionId) throw new Error('MissingNativeSessionIdentity');
      if (input.resume && sessionId !== input.resume.native_id) throw new Error('SessionIdentityMismatch');
      const modelsObject = object(result['models'] ?? {});
      const rawModels = Array.isArray(modelsObject['availableModels']) ? modelsObject['availableModels'] : [];
      const availableModels = rawModels.filter(model => typeof object(model)['modelId'] === 'string').map(model => object(model)['modelId'] as string);
      const ref: SessionRef = {
        harness: this.#config.harness,
        native_id: sessionId,
        native_version: typeof agentInfo['version'] === 'string' ? agentInfo['version'] : '',
        project_id: input.project_id,
        cwd: input.cwd,
        resolved_route: input.route,
      };
      const session: AcpSession = { input, ref, identity: canonical(ref), process, client, sessionId, agentName, availableModels, approvals: new Map(), closed: false };
      this.#session = session;
      return session;
    } catch (error) {
      await client.close();
      throw error instanceof Error ? error : new Error('AcpOpenFailed');
    }
  }

  async open(input: OpenSessionInput): Promise<SessionRef> {
    const { version, reason } = await this.#probeVersion();
    if (version === '') throw new Error(`${this.#config.harness}Unavailable: ${reason.slice(0, 160)}`);
    const session = await this.#openSession(input);
    const modelPinned = input.route.model_id === 'auto' || session.availableModels.length === 0 || session.availableModels.includes(input.route.model_id);
    await this.host.recordNativeEvent({
      event: 'session_bound',
      harness: this.#config.harness,
      native_id: session.sessionId,
      agent: session.agentName,
      native_version: session.ref.native_version,
      route_model_pinned: modelPinned,
    }, input).catch(() => undefined);
    return session.ref;
  }

  #bound(ref: SessionRef): AcpSession {
    if (!this.#session || this.#session.closed || this.#session.identity !== canonical(ref)) throw new Error('SessionIdentityMismatch');
    return this.#session;
  }

  #workerEvent(session: AcpSession, turnId: string, payload: WorkerEvent['payload'], toolId?: string): WorkerEvent {
    return { run_id: session.input.run_id, session_id: session.sessionId, turn_id: turnId, tool_id: toolId, payload };
  }

  #pickOption(options: Array<Record<string, unknown>>, allowed: boolean): Record<string, unknown> | undefined {
    const kindOf = (option: Record<string, unknown>): string => (typeof option['kind'] === 'string' ? option['kind'] : '');
    if (allowed) {
      const allow = options.filter(option => kindOf(option).startsWith('allow'));
      const preferred = allow.find(option => kindOf(option) === 'allow_once' || kindOf(option) === 'allow_session');
      return preferred ?? allow[0];
    }
    const deny = options.find(option => kindOf(option).startsWith('reject') || kindOf(option).startsWith('deny'));
    return deny;
  }

  #handleServerRequest(session: AcpSession, item: Extract<RpcStreamItem, { kind: 'server_request' }>, turnId: string, push: (event: WorkerEvent) => void): void {
    const params = object(item.params);
    if (item.method === 'session/request_permission') {
      const toolCall = object(params['toolCall'] ?? {});
      const toolCallId = typeof toolCall['toolCallId'] === 'string' ? toolCall['toolCallId'] : '';
      const rawOptions = Array.isArray(params['options']) ? params['options'] : [];
      const options = rawOptions.filter(option => typeof option === 'object' && option !== null) as Array<Record<string, unknown>>;
      const request: NativePermissionRequest = {
        request_id: String(item.id),
        session_id: session.sessionId,
        name: 'acp.toolPermission',
        tool_call_id: toolCallId,
        input: { name: toolCall['name'], title: toolCall['title'], kind: toolCall['kind'], rawInput: toolCall['rawInput'] },
        raw: params,
      };
      void (async () => {
        const outcome = await new Promise<{ allowed: boolean; optionId?: string }>(async (resolve) => {
          let action: ActionSpec;
          try {
            action = await this.host.prepareAction(request, session.input);
          } catch (error) {
            this.host.recordNativeEvent({ event: 'approval_failed_closed', method: item.method, item_id: toolCallId, reason: error instanceof Error ? error.message : String(error) }, session.input).catch(() => undefined);
            resolve({ allowed: false });
            return;
          }
          session.approvals.set(request.request_id, { request, action, resolve });
          push(this.#workerEvent(session, turnId, { type: 'approval_needed', action }, toolCallId || undefined));
        });
        if (!outcome.allowed) {
          const option = this.#pickOption(options, false);
          await session.client.reply(item.id, { outcome: option ? { outcome: 'selected', optionId: option['optionId'] } : { outcome: 'cancelled' } }).catch(() => undefined);
        } else {
          const option = this.#pickOption(options, true);
          if (!option) {
            await session.client.reply(item.id, { outcome: { outcome: 'cancelled' } }).catch(() => undefined);
          } else {
            await session.client.reply(item.id, { outcome: { outcome: 'selected', optionId: option['optionId'] } }).catch(() => undefined);
          }
        }
        await this.host.recordNativeEvent({ event: 'approval_answered', method: item.method, item_id: toolCallId, allowed: outcome.allowed }, session.input).catch(() => undefined);
      })();
      return;
    }
    // Unrecognised server request: fail closed — deny and record.
    if (LIVE_FS_CLIENT && (item.method === 'fs/write_text_file' || item.method === 'fs/read_text_file')) {
      void this.#handleFsRequest(session, item.id, item.method, params).catch(async error => {
        const reason = error instanceof Error ? error.message : String(error);
        await session.client.replyError(item.id, -32603, reason).catch(() => undefined);
        await this.host.recordNativeEvent({ event: 'fs_request_denied', method: item.method, reason }, session.input).catch(() => undefined);
      });
      return;
    }
    void session.client.reply(item.id, { denied: true }).catch(() => undefined);
    void this.host.recordNativeEvent({ event: 'server_request_denied', method: item.method }, session.input).catch(() => undefined);
  }

  /** Serve one ACP client file request within the session worktree only. */
  async #handleFsRequest(session: AcpSession, id: number, method: string, params: Record<string, unknown>): Promise<void> {
    const requested = typeof params['path'] === 'string' ? params['path'] : '';
    const path = workspacePath(session.input.cwd, requested);
    if (method === 'fs/write_text_file') {
      const content = typeof params['content'] === 'string' ? params['content'] : '';
      await mkdir(dirname(path), { recursive: true });
      const handle = await open(path, 'w');
      try {
        await handle.writeFile(content, 'utf8');
        await handle.sync();
      } finally {
        await handle.close();
      }
      await session.client.reply(id, null);
      await this.host.recordNativeEvent({ event: 'fs_write_landed', session_id: session.sessionId, path, bytes: Buffer.byteLength(content) }, session.input).catch(() => undefined);
      return;
    }
    const text = await readFile(path, 'utf8');
    await session.client.reply(id, { content: text });
    await this.host.recordNativeEvent({ event: 'fs_read_served', session_id: session.sessionId, path }, session.input).catch(() => undefined);
  }

  #applyUpdate(session: AcpSession, update: Record<string, unknown>, push: (event: WorkerEvent) => void, turnId: string, messageId: string): void {
    const kind = typeof update['sessionUpdate'] === 'string' ? update['sessionUpdate'] : '';
    if (kind === 'user_message_chunk') {
      const content = object(update['content'] ?? {});
      const text = typeof content['text'] === 'string' ? content['text'] : '';
      if (text) push(this.#workerEvent(session, turnId, { type: 'text_delta', message_id: messageId, delta: text }));
    } else if (kind === 'agent_message_chunk') {
      const content = object(update['content'] ?? {});
      const text = typeof content['text'] === 'string' ? content['text'] : '';
      if (text) push(this.#workerEvent(session, turnId, { type: 'text_delta', message_id: messageId, delta: text }));
    } else if (kind === 'agent_thought_chunk') {
      const content = object(update['content'] ?? {});
      const text = typeof content['text'] === 'string' ? content['text'] : '';
      if (text) push(this.#workerEvent(session, turnId, { type: 'reasoning_delta', message_id: messageId, delta: text }));
    } else if (kind === 'tool_call' || kind === 'tool_call_update') {
      const toolCallId = typeof update['toolCallId'] === 'string' ? update['toolCallId'] : '';
      const name = typeof update['name'] === 'string' ? update['name'] : '';
      const kindLabel = typeof update['kind'] === 'string' ? update['kind'] : 'other';
      const status = typeof update['status'] === 'string' ? update['status'] : 'pending';
      if (status === 'pending' || status === 'in_progress') {
        const action = deriveActionSpec(session.input, toolCallId || name, `acp.tool.${kindLabel}`, { name, title: update['title'], rawInput: update['rawInput'] });
        push(this.#workerEvent(session, turnId, { type: 'tool_started', action }, toolCallId || name || undefined));
      } else if (status === 'completed' || status === 'failed') {
        push(this.#workerEvent(session, turnId, { type: 'tool_finished', action_id: `act_${digest({ toolId: toolCallId || name, kind: kindLabel })}`, result: { status, content: update['content'] ?? null, rawOutput: update['rawOutput'] ?? null } }, toolCallId || name || undefined));
      }
    }
    // Other sessionUpdate variants (plan, usage, available commands) are informational.
  }

  async *turn(session: SessionRef, input: TurnInput): AsyncIterable<WorkerEvent> {
    const bound = this.#bound(session);
    if (bound.activeTurn) throw new Error('AcpTurnAlreadyActive');
    checkTurnBudget(bound, input);
    await this.host.state(bound.input, input);
    const parts = input.content.filter(part => part.type === 'text');
    if (parts.length === 0) throw new Error('AcpTurnRequiresTextContent');
    const prompt = [contextPrefix(bound.input.context), ...parts.map(part => (part.type === 'text' ? part.text : ''))].join('\n\n');
    const { promise, resolve } = Promise.withResolvers<{ ok: boolean; stop_reason?: string; message?: string }>();
    const turnState: AcpTurnState = { message_id: input.message_id, stop: { promise, resolve } };
    bound.activeTurn = turnState;
    const out: Array<WorkerEvent> = [];
    let outputReady = Promise.withResolvers<void>();
    const push = (event: WorkerEvent): void => { out.push(event); outputReady.resolve(); };
    const reading = new AbortController();
    const stopped = turnState.stop.promise.then(stop => ({ kind: 'stop' as const, stop }));
    let pending: Promise<{ kind: 'item'; item: RpcStreamItem }> | undefined;
    let completed = false;
    bound.client.request('session/prompt', { sessionId: bound.sessionId, prompt: [{ type: 'text', text: prompt }] }, ACP_TURN_WINDOW_MS).then(result => {
      const resultObject = object(result);
      const stopReason = typeof resultObject['stopReason'] === 'string' ? resultObject['stopReason'] : undefined;
      resolve({ ok: stopReason === 'end_turn', stop_reason: stopReason });
      return undefined;
    }).catch(error => {
      resolve({ ok: false, message: error instanceof Error ? error.message : String(error) });
      return undefined;
    });
    try {
      yield this.#workerEvent(bound, input.message_id, { type: 'turn_started', message_id: input.message_id });
      for (;;) {
        while (out.length > 0) yield out.shift()!;
        outputReady = Promise.withResolvers<void>();
        pending ??= bound.client.queue.take(ACP_QUEUE_TAKE_MS, reading.signal).then(item => ({ kind: 'item' as const, item }));
        const next = await Promise.race([
          pending,
          stopped,
          outputReady.promise.then(() => ({ kind: 'output' as const })),
        ]);
        if (next.kind === 'stop') { completed = true; break; }
        if (next.kind === 'output') continue;
        pending = undefined;
        const item = next.item;
        if (item.kind === 'server_request') {
          this.#handleServerRequest(bound, item, input.message_id, push);
          continue;
        }
        const params = object(item.params);
        const sessionId = typeof params['sessionId'] === 'string' ? params['sessionId'] : '';
        if (sessionId && sessionId !== bound.sessionId) continue;
        const update = object(params['update'] ?? {});
        this.#applyUpdate(bound, update, push, input.message_id, input.message_id);
      }
    } catch (error) {
      resolve({ ok: false, message: error instanceof Error ? error.message : String(error) });
    } finally {
      reading.abort(new Error('turn_consumption_stopped'));
      await pending?.catch(() => undefined);
      if (!completed && !bound.closed) await this.close(session);
    }
    const stop = await turnState.stop.promise;
    const status = stop.stop_reason === 'cancelled' ? 'cancelled' : stop.ok ? 'succeeded' : 'failed';
    const stopReason = (stop.stop_reason ?? stop.message ?? 'turn_aborted').slice(0, 200);
    bound.activeTurn = undefined;
    while (out.length > 0) yield out.shift()!;
    yield this.#workerEvent(bound, input.message_id, { type: 'turn_finished', message_id: input.message_id, status, stop_reason: stopReason });
  }

  async answerPermission(answer: PermissionAnswer): Promise<void> {
    const session = this.#session;
    if (!session || session.closed) throw new Error('AcpNoOpenSession');
    const record = session.approvals.get(answer.native_request_id);
    if (!record) throw new Error('UnknownNativePermission');
    const verified = await this.host.verifyAnswer(answer, record.request);
    if (!verified) throw new Error('PermissionAnswerRejected');
    record.resolve({ allowed: answer.decision === 'allow' });
  }

  async interrupt(session: SessionRef): Promise<void> {
    const bound = this.#bound(session);
    const activeTurn = bound.activeTurn;
    if (!activeTurn) throw new Error('AcpNoActiveTurn');
    await bound.client.notify('session/cancel', { sessionId: bound.sessionId });
    const stop = await activeTurn.stop.promise;
    if (stop.stop_reason !== 'cancelled') throw new Error('AcpCancellationNotObserved');
  }

  async exportContext(session: SessionRef): Promise<ContextPackage> {
    const bound = this.#bound(session);
    if (bound.activeTurn) throw new Error('AcpTurnAlreadyActive');
    const resultRaw = await bound.client.request('session/load', { sessionId: bound.sessionId, cwd: bound.input.cwd, mcpServers: [] }, 60_000);
    object(resultRaw);
    const brief: string[] = [];
    const append = (text: string): void => {
      if (text && brief.join('\n').length < 48_000) brief.push(text);
    };
    const deadline = Date.now() + 8_000;
    for (;;) {
      let item: RpcStreamItem;
      try {
        item = await bound.client.queue.take(1_500);
      } catch {
        break;
      }
      if (item.kind === 'notification') {
        const params = object(item.params);
        const sessionId = typeof params['sessionId'] === 'string' ? params['sessionId'] : '';
        if (sessionId && sessionId !== bound.sessionId) continue;
        const update = object(params['update'] ?? {});
        const kind = typeof update['sessionUpdate'] === 'string' ? update['sessionUpdate'] : '';
        const content = object(update['content'] ?? {});
        const text = typeof content['text'] === 'string' ? content['text'] : '';
        if (kind === 'user_message_chunk' && text) append(`user: ${text.slice(0, 2_000)}`);
        else if (kind === 'agent_message_chunk' && text) append(`assistant: ${text.slice(0, 2_000)}`);
      }
      if (Date.now() > deadline) break;
    }
    const text = brief.join('\n');
    return {
      digest: digest(text),
      brief: text || `acp session ${bound.sessionId} (no replayed content)`,
      constraints: [`agent=${bound.agentName}`],
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
    for (const record of bound.approvals.values()) record.resolve({ allowed: false });
    bound.approvals.clear();
    if (bound.activeTurn) {
      bound.activeTurn.stop.resolve({ ok: false, stop_reason: 'session_closed' });
      bound.activeTurn = undefined;
    }
    await bound.client.close();
    const exit = await bound.process.exit;
    await this.host.recordNativeEvent({ event: 'session_closed', harness: this.#config.harness, native_id: session.native_id, exit: describeExit(exit.code, exit.signal) }, bound.input).catch(() => undefined);
  }
}
