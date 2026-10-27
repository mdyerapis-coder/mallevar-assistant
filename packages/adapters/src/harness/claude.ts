import type { ActionSpec, CapabilityState, CapabilityObservation, ContextPackage, HarnessDescriptor, OpenSessionInput, PermissionAnswer, SessionRef, TurnInput, WorkerEvent } from '../../../contracts/src/generated';
import { formatWireI64 } from '../../../contracts/src/wire';
import { readFileSync, existsSync, mkdirSync } from 'node:fs';
import { resolve } from 'node:path';
import { canonical, checkTurnBudget, contextPrefix, deriveActionSpec, digest, type NativePermissionHost, type NativePermissionRequest } from './native/host';
import { NATIVE_STATE_ROOT, describeExit, runProbe, spawnNativeProcess, workerEnv, type NativeProcess, type SpawnOptions } from './native/process';
import { versionFromProbe } from './native/rpc';
import type { HarnessAdapter } from './types';

/** Fail-closed refusal raised before any native session is dispatched when the adapter's own
 *  managed Claude settings cannot be trusted. The stable `code` distinguishes an unparseable file
 *  (`settings_invalid`) from a settings file that declares an unmanaged permission default
 *  (`settings_unsafe_permission_mode`). Print/stream mode silently ignores invalid settings, so
 *  both must block the launch rather than let the engine continue with widened defaults. */
export class ClaudeSettingsError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = 'ClaudeSettingsError';
    this.code = code;
  }
}

/** Permission defaults that hand the native engine authority the plan forbids: auto-approval,
 *  edit auto-accept and any variant of a bypass. Matched after normalisation so
 *  `accept-edits`/`dangerously-skip-permissions`/`bypassPermissions` all collapse onto one form. */
const CLAUDE_UNSAFE_PERMISSION_MODES: Record<string, true> = {
  auto: true,
  acceptedits: true,
  allowall: true,
  alloweverything: true,
  bypasspermissions: true,
  dangerouslyskippermissions: true,
  dangerouslybypasspermissions: true,
  dangerouslybypassapprovalsandsandbox: true,
};

function isUnsafePermissionMode(value: unknown): boolean {
  if (typeof value !== 'string') return false;
  const normalized = value.trim().toLowerCase().replace(/[^a-z0-9]/g, '');
  if (normalized === '') return false;
  if (CLAUDE_UNSAFE_PERMISSION_MODES[normalized]) return true;
  return normalized.includes('bypass') || normalized.includes('danger') || normalized.startsWith('skip');
}

/** Every field a managed settings file could use to declare its permission default. */
function declaredPermissionModes(settings: Record<string, unknown>): unknown[] {
  const declared: unknown[] = [settings['defaultMode'], settings['default_mode'], settings['permissionMode'], settings['permission-mode']];
  const permissions = settings['permissions'];
  if (permissions && typeof permissions === 'object' && !Array.isArray(permissions)) {
    const record = permissions as Record<string, unknown>;
    declared.push(record['defaultMode'], record['default_mode']);
  }
  return declared;
}

/** Pull one newline-delimited stdout frame, failing closed with a typed adapter timeout. */
async function readFrame(frames: AsyncIterator<unknown>, timeoutMs: number, timeoutCode: string): Promise<unknown | null> {
  let handle: NodeJS.Timeout | undefined;
  const timeout = new Promise<never>((_resolve, reject) => { handle = setTimeout(() => reject(new Error(timeoutCode)), timeoutMs); });
  try {
    const result = await Promise.race([frames.next(), timeout]);
    return result.done ? null : result.value;
  } finally {
    clearTimeout(handle);
  }
}

type ClaudeTurnState = { message_id: string; stop: { promise: Promise<void>; resolve: () => void } };

type ClaudeSession = {
  input: OpenSessionInput;
  ref: SessionRef;
  process: NativeProcess;
  frames: AsyncIterator<unknown>;
  sessionId: string;
  initModel: string;
  activeTurn?: ClaudeTurnState;
  approvals: Map<string, { request: NativePermissionRequest; resolve: (allowed: boolean) => void }>;
  closed: boolean;
  transcriptPath: string;
};

/** Genuine Claude Code (stream-json over stdio) adapter.
 *  Host-permission model: can_use_tool control_requests are forwarded to the core; unknown control
 *  requests are denied (fail closed). */
export class ClaudeAdapter implements HarnessAdapter {
  #binary: string;
  #home: string;
  #version = '';
  #session?: ClaudeSession;

  constructor(readonly host: NativePermissionHost, options: { binary?: string; home?: string } = {}) {
    this.#binary = options.binary ?? 'claude';
    this.#home = options.home ?? `${NATIVE_STATE_ROOT}/claude-home`;
  }

  #spawnOptions(cwd: string): SpawnOptions {
    return { cwd, env: { ...workerEnv(), HOME: this.#home, CLAUDE_CONFIG_DIR: this.#home } };
  }

  async #probeVersion(): Promise<{ version: string; reason: string }> {
    mkdirSync(this.#home, { recursive: true });
    const probe = await runProbe(this.#binary, ['--version'], { cwd: this.#home }, 10_000).catch(error => ({ exit: null, signal: null, stdout: '', stderr: String(error) }));
    const version = versionFromProbe(probe, [/claude\s+v?(\d+\.\d+\.\d+[\w.-]*)/i, /(\d+\.\d+\.\d+)/]);
    if (probe.exit !== 0 || version === '') return { version: '', reason: `claude --version ${describeExit(probe.exit, probe.signal)}: ${probe.stderr.slice(0, 200)}` };
    return { version, reason: '' };
  }

  async describe(): Promise<HarnessDescriptor> {
    const { version, reason } = await this.#probeVersion();
    const state: CapabilityState = version === '' ? 'unavailable' : 'unqualified';
    const implementation = version === '' ? `unavailable:${reason.slice(0, 120)}` : `probed:claude ${version}`;
    const observation = (name: string): CapabilityObservation => ({ name, state, implementation_version: implementation, observed_at: formatWireI64(BigInt(Date.now())) });
    return {
      harness: 'claude',
      version,
      transport: 'stdio-stream-json',
      route_compatibility: ['native-stdio-claude-host-permission'],
      approval: state,
      cancel: state,
      resume: state,
      child_observability: state,
      capabilities: [observation('approval'), observation('cancel'), observation('resume'), observation('child_observability')],
    };
  }

  async #openSession(input: OpenSessionInput): Promise<ClaudeSession> {
    if (this.#session && !this.#session.closed) throw new Error('ClaudeAlreadyOpen');
    if (input.route.harness && input.route.harness !== 'claude') throw new Error('SessionIdentityMismatch');
    if (input.resume && (input.resume.harness !== 'claude' || input.resume.project_id !== input.project_id || input.resume.cwd !== input.cwd || canonical(input.resume.resolved_route) !== canonical(input.route))) throw new Error('SessionIdentityMismatch');
    mkdirSync(this.#home, { recursive: true });
    const sessionId = crypto.randomUUID();
    const args = [
      '-p',
      '--verbose',
      '--output-format', 'stream-json',
      '--input-format', 'stream-json',
      // Claude 2.1.296 has no `--permission-prompt-tool` flag; the host-answered prompt in
      // print/stream mode is `--permission-prompts host`. Never pass a flag with a missing value,
      // or the next token (here `--permission-mode`) is swallowed as its value.
      '--permission-prompts', 'host',
      '--permission-mode', 'manual',
      '--session-id', sessionId,
    ];
    const model = input.route.model_id === 'auto' ? undefined : input.route.model_id;
    if (model) { args.push('--model', model); }
    if (input.resume) { args.push('--resume', input.resume.native_id); }
    if (input.policy.allowed_tools.length > 0) { args.push('--allowed-tools', input.policy.allowed_tools.join(',')); }
    const process = spawnNativeProcess(this.#binary, args, this.#spawnOptions(input.cwd));
    const frames = process.frames()[Symbol.asyncIterator]();
    let initFrame: Record<string, unknown> | null = null;
    try {
      while (initFrame === null) {
        const frame = await readFrame(frames, 30_000, 'ClaudeInitTimeout');
        if (frame === null) throw new Error('ClaudeInitFailed');
        const record = frame as Record<string, unknown>;
        if (record['type'] === 'system' && record['subtype'] === 'init') initFrame = record;
      }
    } catch (error) {
      await process.kill(true).catch(() => undefined);
      throw error;
    }
    const initSessionId = typeof initFrame['session_id'] === 'string' ? initFrame['session_id'] : '';
    if (!initSessionId || initSessionId !== sessionId) throw new Error('SessionIdentityMismatch');
    const initModel = typeof initFrame['model'] === 'string' ? initFrame['model'] : '';
    if (model && initModel && initModel !== model) throw new Error('SessionIdentityMismatch');
    const transcriptPath = this.#findTranscript(this.#home, input.cwd, sessionId);
    const ref: SessionRef = {
      harness: 'claude',
      native_id: sessionId,
      native_version: this.#version,
      project_id: input.project_id,
      cwd: input.cwd,
      resolved_route: input.route,
    };
    const session: ClaudeSession = { input, ref, process, frames, sessionId, initModel, approvals: new Map(), closed: false, transcriptPath };
    this.#session = session;
    return session;
  }

  #findTranscript(home: string, cwd: string, sessionId: string): string {
    const projectsDir = resolve(home, 'projects');
    if (!existsSync(projectsDir)) return '';
    const escapedCwd = cwd.replace(/[^a-zA-Z0-9]+/g, '-');
    const projectDir = resolve(projectsDir, escapedCwd);
    const transcriptFile = resolve(projectDir, `${sessionId}.jsonl`);
    return existsSync(transcriptFile) ? transcriptFile : '';
  }

  /** Refuse to launch when the adapter's own managed settings cannot be trusted. Claude's
   *  print/stream mode silently ignores invalid settings and would fall back to widened defaults,
   *  so a malformed file or an unmanaged permission default must block the native session. */
  #validateManagedSettings(): void {
    const settingsPath = resolve(this.#home, 'settings.json');
    if (!existsSync(settingsPath)) return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(readFileSync(settingsPath, 'utf-8'));
    } catch (error) {
      throw new ClaudeSettingsError('settings_invalid', `ClaudeSettingsInvalid: ${settingsPath} is not valid JSON: ${error instanceof Error ? error.message : String(error)}`);
    }
    if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
      throw new ClaudeSettingsError('settings_invalid', `ClaudeSettingsInvalid: ${settingsPath} must contain a JSON object`);
    }
    const settings = parsed as Record<string, unknown>;
    const permissions = settings['permissions'];
    if (permissions !== undefined && (permissions === null || typeof permissions !== 'object' || Array.isArray(permissions))) {
      throw new ClaudeSettingsError('settings_invalid', `ClaudeSettingsInvalid: ${settingsPath} "permissions" must be an object`);
    }
    for (const mode of declaredPermissionModes(settings)) {
      if (isUnsafePermissionMode(mode)) {
        throw new ClaudeSettingsError('settings_unsafe_permission_mode', `ClaudeSettingsUnsafePermissions: ${settingsPath} declares unmanaged permission mode ${JSON.stringify(mode)}`);
      }
    }
  }

  async open(input: OpenSessionInput): Promise<SessionRef> {
    this.#validateManagedSettings();
    const { version, reason } = await this.#probeVersion();
    if (version === '') throw new Error(`ClaudeUnavailable: ${reason.slice(0, 160)}`);
    this.#version = version;
    const session = await this.#openSession(input);
    await this.host.recordNativeEvent({ event: 'session_bound', harness: 'claude', native_id: session.sessionId, native_version: version, init_model: session.initModel }, input).catch(() => undefined);
    return session.ref;
  }

  #bound(ref: SessionRef): ClaudeSession {
    if (!this.#session || this.#session.closed || this.#session.ref.native_id !== ref.native_id || canonical(this.#session.ref.resolved_route) !== canonical(ref.resolved_route)) throw new Error('SessionIdentityMismatch');
    return this.#session;
  }

  #workerEvent(session: ClaudeSession, turnId: string, payload: WorkerEvent['payload'], toolId?: string): WorkerEvent {
    return { run_id: session.input.run_id, session_id: session.sessionId, turn_id: turnId, tool_id: toolId, payload };
  }

  /** Bridge one native frame. Returns true when the frame terminated the turn. */
  #dispatchFrame(bound: ClaudeSession, messageId: string, frame: Record<string, unknown>, push: (event: WorkerEvent) => void, turnState: ClaudeTurnState): boolean {
    if (frame['type'] === 'control_request') {
      const requestId = typeof frame['request_id'] === 'string' ? frame['request_id'] : '';
      const request = (frame['request'] ?? {}) as Record<string, unknown>;
      const subtype = typeof request['subtype'] === 'string' ? request['subtype'] : '';
      if (subtype === 'can_use_tool') {
        const toolName = typeof request['tool_name'] === 'string' ? request['tool_name'] : '';
        const toolInput = request['input'] ?? {};
        const toolUseId = typeof request['tool_use_id'] === 'string' ? request['tool_use_id'] : '';
        const nativeRequest: NativePermissionRequest = {
          request_id: requestId,
          session_id: bound.sessionId,
          name: 'claude.canUseTool',
          tool_call_id: toolUseId || toolName,
          input: { tool_name: toolName, input: toolInput },
          raw: request,
        };
        void (async () => {
          let action: ActionSpec;
          try {
            action = await this.host.prepareAction(nativeRequest, bound.input);
          } catch (error) {
            await bound.process.writeFrame({
              type: 'control_response',
              response: { subtype: 'error', request_id: requestId, error: error instanceof Error ? error.message : String(error) },
            }).catch(() => undefined);
            await this.host.recordNativeEvent({ event: 'approval_failed_closed', method: subtype, item_id: toolUseId || toolName, reason: error instanceof Error ? error.message : String(error) }, bound.input).catch(() => undefined);
            return;
          }
          const { promise, resolve } = Promise.withResolvers<boolean>();
          bound.approvals.set(requestId, { request: nativeRequest, resolve });
          push(this.#workerEvent(bound, messageId, { type: 'approval_needed', action }, toolUseId || toolName || undefined));
          let allowed = false;
          try {
            allowed = await promise;
          } finally {
            bound.approvals.delete(requestId);
            await bound.process.writeFrame({
              type: 'control_response',
              response: { subtype: 'success', request_id: requestId, response: allowed ? { behavior: 'allow', updatedInput: toolInput } : { behavior: 'deny', message: 'ironbark denied' } },
            }).catch(() => undefined);
            await this.host.recordNativeEvent({ event: 'approval_answered', method: subtype, item_id: toolUseId || toolName, allowed }, bound.input).catch(() => undefined);
          }
        })().catch(() => undefined);
      } else if (subtype === 'interrupt') {
        void bound.process.writeFrame({ type: 'control_response', response: { subtype: 'success', request_id: requestId } }).catch(() => undefined);
        turnState.stop.resolve();
      } else {
        void bound.process.writeFrame({ type: 'control_response', response: { subtype: 'error', request_id: requestId, error: 'unmanaged native control request denied' } }).catch(() => undefined);
        void this.host.recordNativeEvent({ event: 'control_request_denied', subtype }, bound.input).catch(() => undefined);
      }
      return false;
    }
    if (frame['type'] === 'assistant') {
      const message = frame['message'] as { content?: unknown } | undefined;
      const content = Array.isArray(message?.content) ? (message.content as Array<Record<string, unknown>>) : [];
      for (const block of content) {
        const text = block['text'];
        if (block['type'] === 'text' && typeof text === 'string' && text) {
          push(this.#workerEvent(bound, messageId, { type: 'text_delta', message_id: messageId, delta: text }));
        } else if (block['type'] === 'tool_use') {
          const toolId = typeof block['id'] === 'string' ? block['id'] : '';
          const toolName = typeof block['name'] === 'string' ? block['name'] : '';
          const toolInput = block['input'] ?? {};
          const action = deriveActionSpec(bound.input, toolId || toolName, `claude.tool.${toolName}`, toolInput);
          push(this.#workerEvent(bound, messageId, { type: 'tool_started', action }, toolId || toolName || undefined));
        }
      }
      return false;
    }
    if (frame['type'] === 'user') {
      const message = frame['message'] as { content?: unknown } | undefined;
      const content = Array.isArray(message?.content) ? (message.content as Array<Record<string, unknown>>) : [];
      for (const block of content) {
        if (block['type'] === 'tool_result') {
          const toolUseId = typeof block['tool_use_id'] === 'string' ? block['tool_use_id'] : '';
          const isError = block['is_error'] === true;
          const resultContent = block['content'];
          push(this.#workerEvent(bound, messageId, { type: 'tool_finished', action_id: `act_${digest({ toolId: toolUseId, kind: 'tool_result' })}`, result: { isError, content: resultContent } }, toolUseId || undefined));
        }
      }
      return false;
    }
    if (frame['type'] === 'result') {
      const isError = frame['is_error'] === true;
      const terminalReason = typeof frame['terminal_reason'] === 'string' ? frame['terminal_reason'] : undefined;
      const stopReason = terminalReason ?? (isError ? 'error' : 'end_turn');
      const status = stopReason === 'interrupted' ? 'cancelled' : isError ? 'failed' : 'succeeded';
      push(this.#workerEvent(bound, messageId, { type: 'turn_finished', message_id: messageId, status, stop_reason: stopReason }));
      return true;
    }
    return false;
  }

  async *turn(session: SessionRef, input: TurnInput): AsyncIterable<WorkerEvent> {
    const bound = this.#bound(session);
    if (bound.activeTurn) throw new Error('ClaudeTurnAlreadyActive');
    checkTurnBudget(bound, input);
    await this.host.state(bound.input, input);
    const parts = input.content.filter(part => part.type === 'text');
    if (parts.length === 0) throw new Error('ClaudeTurnRequiresTextContent');
    const prompt = [contextPrefix(bound.input.context), ...parts.map(part => (part.type === 'text' ? part.text : ''))].join('\n\n');
    const out: Array<WorkerEvent> = [];
    const push = (event: WorkerEvent): void => { out.push(event); };
    const { promise, resolve } = Promise.withResolvers<void>();
    const turnState: ClaudeTurnState = { message_id: input.message_id, stop: { promise, resolve } };
    bound.activeTurn = turnState;
    try {
      push(this.#workerEvent(bound, input.message_id, { type: 'turn_started', message_id: input.message_id }));
      await bound.process.writeFrame({
        type: 'user',
        message: { role: 'user', content: [{ type: 'text', text: prompt }] },
        parent_tool_use_id: null,
        session_id: bound.sessionId,
      });
      const stopSignal = promise.then(() => 'stop' as const);
      let finished = false;
      while (!finished) {
        const raced: unknown = await Promise.race([readFrame(bound.frames, 120_000, 'ClaudeTurnTimeout'), stopSignal]);
        if (raced === 'stop' || raced === null) break;
        finished = this.#dispatchFrame(bound, input.message_id, raced as Record<string, unknown>, push, turnState);
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      push(this.#workerEvent(bound, input.message_id, { type: 'turn_finished', message_id: input.message_id, status: 'failed', stop_reason: message.slice(0, 200) }));
    }
    bound.activeTurn = undefined;
    for (const event of out) yield event;
  }

  async answerPermission(answer: PermissionAnswer): Promise<void> {
    const session = this.#session;
    if (!session || session.closed) throw new Error('ClaudeNoOpenSession');
    const record = session.approvals.get(answer.native_request_id);
    if (!record) throw new Error('UnknownNativePermission');
    const verified = await this.host.verifyAnswer(answer, record.request);
    if (!verified) throw new Error('PermissionAnswerRejected');
    record.resolve(answer.decision === 'allow');
  }

  async interrupt(session: SessionRef): Promise<void> {
    const bound = this.#bound(session);
    if (!bound.activeTurn) throw new Error('ClaudeNoActiveTurn');
    const requestId = `irq_${Date.now()}`;
    void bound.process.writeFrame({
      type: 'control_request',
      request_id: requestId,
      request: { subtype: 'interrupt' },
    }).catch(() => undefined);
    bound.activeTurn.stop.resolve();
  }

  async exportContext(session: SessionRef): Promise<ContextPackage> {
    const bound = this.#bound(session);
    if (!bound.transcriptPath || !existsSync(bound.transcriptPath)) return {
      digest: digest(''),
      brief: `claude session ${bound.sessionId} (no transcript found)`,
      constraints: [`model=${bound.initModel}`],
      excerpts: [],
      artifacts: [],
      unresolved_questions: [],
      originating_sessions: [bound.ref],
    };
    const brief: string[] = [];
    for (const line of readFileSync(bound.transcriptPath, 'utf-8').split('\n')) {
      if (!line.trim()) continue;
      try {
        const frame = JSON.parse(line) as { type?: string; message?: { content?: unknown } };
        const role = frame.type === 'user' ? 'user' : frame.type === 'assistant' ? 'assistant' : '';
        if (!role) continue;
        const content = Array.isArray(frame.message?.content) ? (frame.message.content as Array<Record<string, unknown>>) : [];
        const text = content.filter(block => block['type'] === 'text' && typeof block['text'] === 'string').map(block => String(block['text'])).join(' ');
        if (text) brief.push(`${role}: ${text.slice(0, 2_000)}`);
        if (brief.join('\n').length > 48_000) break;
      } catch {
        // skip malformed lines
      }
    }
    return {
      digest: digest(brief.join('\n')),
      brief: brief.join('\n').slice(0, 48_000),
      constraints: [`model=${bound.initModel}`],
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
    if (bound.activeTurn) bound.activeTurn.stop.resolve();
    bound.process.stdin.end();
    await bound.process.kill(false);
    const exit = await bound.process.exit;
    await this.host.recordNativeEvent({ event: 'session_closed', harness: 'claude', native_id: session.native_id, exit: describeExit(exit.code, exit.signal) }, bound.input).catch(() => undefined);
  }
}
