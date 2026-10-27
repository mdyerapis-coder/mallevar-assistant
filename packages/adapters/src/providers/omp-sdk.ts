import type { AssistantMessage, Context, ImageContent, Message, Model, ModelSpec, StreamOptions, Tool, TextContent } from '@oh-my-pi/pi-ai';
import { streamOpenAICompletions } from '@oh-my-pi/pi-ai/providers/openai-completions';
import { streamOpenAIResponses } from '@oh-my-pi/pi-ai/providers/openai-responses';
import { streamAnthropic } from '@oh-my-pi/pi-ai/providers/anthropic';
import { streamGoogle } from '@oh-my-pi/pi-ai/providers/google';
import { streamOllama } from '@oh-my-pi/pi-ai/providers/ollama';
import { resolveModelPolicy } from '@oh-my-pi/pi-catalog/compat/resolve';
import { NO_AUTH_SENTINEL } from '@oh-my-pi/pi-ai/auth-retry';
import { setTransports } from '@oh-my-pi/pi-utils/logger';
import { CustomAdapters } from './custom.ts';
import type { ConnectionConfig, ConnectionDescriptor, CoreEvent, ProviderAdapter, WorkerEvent, WorkerProviderTurnInput } from '../../../contracts/src/generated.ts';
import { formatWireU64 } from '../../../contracts/src/wire.ts';
import { ProviderRegistry } from './registry.ts';
import { validateEndpoint } from './local.ts';

export type CredentialResolver = (leaseId: string, signal: AbortSignal) => Promise<Uint8Array>;
const emptyUsage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };

function containsCredential(value: unknown, secret: string): boolean {
  if (!secret) return false;
  if (typeof value === 'string') return value.includes(secret);
  if (typeof value !== 'object' || value === null) return false;
  return Object.entries(value).some(([key, child]) => key.includes(secret) || containsCredential(child, secret));
}
function jsonArgumentsEqual(a: unknown, b: unknown): boolean {
  const equal = (x: unknown, y: unknown): boolean => {
    if (x === y) return true;
    if (typeof x !== 'object' || x === null || typeof y !== 'object' || y === null || Array.isArray(x) || Array.isArray(y)) return false;
    const keysX = Object.keys(x as Record<string, unknown>);
    const keysY = Object.keys(y as Record<string, unknown>);
    if (keysX.length !== keysY.length) return false;
    return keysX.every(key => Object.hasOwn(y, key) && equal((x as Record<string, unknown>)[key], (y as Record<string, unknown>)[key]));
  };
  return equal(a, b);
}

function nativeReplayExact(calls: Map<string, { name: string; arguments: unknown }>, canonicalCalls: { id: string; name: string; arguments: unknown }[], blocks: AssistantMessage['content'], payload: unknown): void {
  // openaiResponsesHistory items are the actual transport history: validate
  // their tool calls against the canonical list the same way as blocks.
  if (payload && typeof payload === 'object' && Array.isArray((payload as { items?: unknown }).items)) {
    for (const item of (payload as { items: unknown[] }).items) {
      if (!item || typeof item !== 'object') throw new Error('invalid_native_payload');
      const record = item as Record<string, unknown>;
      if (record.type !== 'function_call' && record.type !== 'custom_tool_call') continue;
      const id = typeof record.call_id === 'string' ? record.call_id : undefined;
      const name = typeof record.name === 'string' ? record.name : undefined;
      let arguments_: unknown = undefined;
      if (typeof record.arguments === 'string') {
        try { arguments_ = JSON.parse(record.arguments); } catch { throw new Error('invalid_native_payload'); }
      } else if (record.arguments !== undefined) {
        arguments_ = record.arguments;
      }
      const canonical = id ? calls.get(id) : undefined;
      if (!canonical || !name || name !== canonical.name || !jsonArgumentsEqual(canonical.arguments, arguments_)) throw new Error('native_tool_replay_mismatch');
    }
  }
  for (const block of blocks) {
    if (block.type !== 'toolCall') continue;
    const canonical = calls.get(block.id);
    if (!canonical || block.name !== canonical.name || !jsonArgumentsEqual(canonical.arguments, block.arguments)) throw new Error('native_tool_replay_mismatch');
  }
  for (const call of canonicalCalls) {
    const occurrences = blocks.filter(block => block.type === 'toolCall' && block.id === call.id).length;
    if (occurrences !== 1) throw new Error('native_tool_replay_mismatch');
  }
}

function contextFor(input: WorkerProviderTurnInput, model: Model): Context {
  const calls = new Map<string, { name: string; arguments: unknown }>();
  const messages: Message[] = [];
  const systemPrompt: string[] = [];
  for (const message of input.messages) {
    const content: (TextContent | ImageContent)[] = message.content.map(part => {
      if (part.type === 'text') return { type: 'text', text: part.text };
      if (part.type !== 'image') throw new Error('unsupported_content');
      // Native core resolves artifacts to bounded inline bytes; worker never fetches arbitrary URIs.
      const match = /^data:(image\/(?:png|jpeg|webp|gif));base64,([A-Za-z0-9+/]+={0,2})$/.exec(part.uri);
      if (!match || match[1] !== part.mime_type || match[2]!.length > 4 * 1024 * 1024) throw new Error('invalid_image');
      return { type: 'image', mimeType: match[1]!, data: match[2]! };
    });
    if (message.role === 'system') {
      if (messages.length || content.some(part => part.type !== 'text')) throw new Error('invalid_system_message');
      systemPrompt.push(content.map(part => (part as TextContent).text).join('\n'));
    } else if (message.role === 'user') messages.push({ role: 'user', content, timestamp: 0 });
    else if (message.role === 'tool') {
      const toolName = message.tool_call_id && calls.get(message.tool_call_id)?.name;
      if (!toolName || !message.tool_call_id) throw new Error('unmatched_tool_result');
      messages.push({ role: 'toolResult', toolCallId: message.tool_call_id, toolName, content, isError: message.tool_result_error ?? false, timestamp: 0 });
    } else {
      for (const call of message.tool_calls ?? []) {
        if (!call.id || !call.name || calls.has(call.id) || typeof call.arguments !== 'object' || call.arguments === null || Array.isArray(call.arguments)) throw new Error('invalid_tool_call');
        calls.set(call.id, { name: call.name, arguments: call.arguments });
      }
      let blocks: AssistantMessage['content'] = [...content, ...(message.tool_calls ?? []).map(call => ({ type: 'toolCall' as const, id: call.id, name: call.name, arguments: call.arguments as Record<string, unknown> }))];
      let providerPayload: AssistantMessage['providerPayload'];
      let requestControls: AssistantMessage['requestControls'];
      if (message.sdk_content !== undefined) {
        if (!message.sdk_route || message.sdk_route.connection_id !== input.route.connection_id || message.sdk_route.model_id !== input.route.model_id
          || typeof message.sdk_content !== 'object' || message.sdk_content === null || Array.isArray(message.sdk_content)) throw new Error('native_replay_route_mismatch');
        const replay = message.sdk_content as Record<string, unknown>;
        if (!Array.isArray(replay.blocks)) throw new Error('invalid_native_replay');
        for (const block of replay.blocks) {
          if (typeof block !== 'object' || block === null || !['text', 'thinking', 'redactedThinking', 'toolCall'].includes(block.type)) throw new Error('unsupported_native_replay');
        }
        blocks = replay.blocks as AssistantMessage['content'];
        if (replay.provider_payload !== undefined) {
          const payload = replay.provider_payload as AssistantMessage['providerPayload'];
          if (!payload || !['openaiResponsesHistory', 'anthropicMessage'].includes(payload.type)) throw new Error('unsupported_native_payload');
          providerPayload = payload;
        }
        requestControls = replay.request_controls as AssistantMessage['requestControls'];
        nativeReplayExact(calls, (message.tool_calls ?? []).map(call => ({ id: call.id, name: call.name, arguments: call.arguments })), blocks, providerPayload);
      }
      messages.push({ role: 'assistant', content: blocks, api: model.api, provider: model.provider, model: model.id,
        usage: structuredClone(emptyUsage), stopReason: message.tool_calls?.length ? 'toolUse' : 'stop', timestamp: 0,
        ...(providerPayload ? { providerPayload } : {}), ...(requestControls ? { requestControls } : {}) });
    }
  }
  const tools: Tool[] = input.tool_schemas.map(tool => {
    if (!/^[A-Za-z0-9_.:-]{1,128}$/.test(tool.name) || typeof tool.input_schema !== 'object' || tool.input_schema === null || Array.isArray(tool.input_schema)) throw new Error('invalid_tool_schema');
    return { name: tool.name, description: tool.description, parameters: tool.input_schema as Tool['parameters'] };
  });
  return { systemPrompt, messages, tools };
}

export class OmpProviderAdapter implements ProviderAdapter {
  private readonly registry: ProviderRegistry;
  constructor(connections: ConnectionConfig[], private readonly credentials: CredentialResolver, private readonly custom = new CustomAdapters()) {
    // Raw SDK diagnostics may include HTTP bodies. Core receives typed sanitised errors.
    setTransports({ file: false, console: false });
    this.registry = new ProviderRegistry(connections);
  }
  async describe(connection: ConnectionConfig): Promise<ConnectionDescriptor> { return this.registry.describe(connection); }
  async *stream(input: WorkerProviderTurnInput, signal: AbortSignal): AsyncIterable<WorkerEvent> {
    const event = (payload: CoreEvent): WorkerEvent => ({ run_id: input.run_id, turn_id: input.request_id, payload });
    let secretBytes: Uint8Array | undefined;
    let secret = '';
    const controller = new AbortController();
    const abort = () => controller.abort();
    signal.addEventListener('abort', abort, { once: true });
    let terminal = false;
    const pendingText = new Map<string, string>();
    try {
      signal.throwIfAborted();
      const extension = input.connection.protocol === 'custom' ? this.custom.get(input.route.connection_id) : undefined;
      const { connection, model: inventory, api } = this.registry.select(input, extension?.implementationVersion);
      if (extension) {
        yield* extension.adapter.stream(input, signal);
        return;
      }
      if (!api) throw new Error('custom_activation_required');
      const spec: ModelSpec = { id: inventory.id, name: inventory.id, api, provider: `ironbark-${connection.id}`,
        baseUrl: connection.endpoint.replace(/\/+$/, ''), reasoning: inventory.capabilities.some(c => c.name === 'reasoning' && c.state === 'qualified'),
        input: inventory.capabilities.some(c => c.name === 'vision' && c.state === 'qualified') ? ['text', 'image'] : ['text'],
        supportsTools: inventory.capabilities.some(c => c.name === 'tools' && c.state === 'qualified'),
        contextWindow: inventory.context_limit, maxTokens: inventory.output_limit,
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, pricingStatus: 'unknown' };
      const policy = resolveModelPolicy(spec);
      const model: Model = { ...spec, identity: policy.identity, compat: policy.compat, thinking: policy.thinking };
      const context = contextFor(input, model);
      secretBytes = await this.credentials(input.credential_lease_id, controller.signal);
      signal.throwIfAborted();
      secret = new TextDecoder('utf-8', { fatal: true }).decode(secretBytes);
      if (secret.length > 8192 || /[\r\n\0]/.test(secret) || (!secret && connection.auth_ref)) throw new Error('credential_unavailable');
      if (containsCredential(context, secret)) throw new Error('credential_disclosure_denied');
      const base = validateEndpoint(connection.endpoint, input.policy.local_only);
      const prefix = base.pathname.replace(/\/+$/, '');
      const path = api === 'openai-completions' ? `${prefix}/chat/completions`
        : api === 'openai-responses' ? `${prefix}/responses`
        : api === 'anthropic-messages' ? `${prefix.replace(/\/v1$/, '')}/v1/messages`
        : api === 'google-generative-ai' ? `${prefix}/models/${inventory.id}:streamGenerateContent`
        : `${prefix.replace(/\/api$/, '')}/api/chat`;
      const expectedPath = new URL(path, base.origin).pathname;
      if (!secret && base.hostname !== '127.0.0.1' && base.hostname !== '[::1]') throw new Error('credential_unavailable');
      let requests = 0;
      let observedInput: number | undefined;
      let observedOutput: number | undefined;
      let reportedCost: number | undefined;
      const guardedFetch: NonNullable<StreamOptions['fetch']> = async (resource, init) => {
        const url = new URL(typeof resource === 'string' ? resource : resource instanceof URL ? resource.href : resource.url);
        const queryAllowed = !url.search || (api === 'google-generative-ai' && url.search === '?alt=sse')
          || (api === 'anthropic-messages' && url.search === '?beta=true');
        if (url.origin !== base.origin || url.pathname !== expectedPath || !queryAllowed || requests++ !== 0
          || (init?.method ?? (resource instanceof Request ? resource.method : 'GET')) !== 'POST') {
          controller.abort();
          throw new Error('pinned_request_denied');
        }
        // The SDK supplies a derived watchdog signal (init.signal or the Request's
        // own signal). Compose it with the adapter controller rather than replacing
        // it, so owner cancellation and the first-event watchdog both abort fetch.
        const callerSignal = init?.signal ?? (resource instanceof Request ? (resource as Request & { signal?: AbortSignal }).signal : undefined);
        const dispatch = callerSignal && typeof AbortSignal.any === 'function'
          ? AbortSignal.any([controller.signal, callerSignal])
          : controller.signal;
        const headers = new Headers(init?.headers ?? (resource instanceof Request ? resource.headers : undefined));
        if (!secret) { headers.delete('authorization'); headers.delete('x-api-key'); headers.delete('x-goog-api-key'); }
        let response: Response;
        try {
          response = await fetch(resource, { ...init, headers, signal: dispatch, redirect: 'error' });
        } catch (error) {
          controller.abort();
          throw error;
        }
        if (!response.ok) {
          // The SDK's transport retry layer is separate from providerRetryWait.
          // Abort this dispatch on its first failed request; only core can retry.
          controller.abort();
          return response;
        }
        if (!response.body) return response;
        let buffered = '';
        let upstreamBytes = 0;
        let outputChars = 0;
        const decoder = new TextDecoder();
        const body = response.body.pipeThrough(new TransformStream<Uint8Array, Uint8Array>({
          transform(chunk, sink) {
            const received = chunk.byteLength;
            if (upstreamBytes + received > 32 * 1024 * 1024) {
              controller.abort();
              throw new Error('provider_response_oversize');
            }
            upstreamBytes += received;
            buffered += decoder.decode(chunk, { stream: true });
            const lines = buffered.split('\n'); buffered = lines.pop()!;
            if (buffered.length > 1024 * 1024) throw new Error('provider_frame_oversize');
            for (let line of lines) {
              if (line.length > 1024 * 1024) throw new Error('provider_frame_oversize');
              if (line.startsWith('data:')) line = line.slice(5).trim();
              if (!line || line === '[DONE]') continue;
              outputChars += line.length + 1;
              if (outputChars > 8 * 1024 * 1024) {
                controller.abort();
                throw new Error('provider_output_oversize');
              }
              try {
                const wire = JSON.parse(line);
                const usage = wire.usage ?? wire.response?.usage ?? wire.usageMetadata ?? wire.message?.usage ?? (wire.done ? wire : undefined);
                if (usage && typeof usage === 'object') {
                  const inputTokens = usage.prompt_tokens ?? usage.input_tokens ?? usage.promptTokenCount ?? usage.prompt_eval_count;
                  const outputTokens = usage.completion_tokens ?? usage.output_tokens ?? usage.candidatesTokenCount ?? usage.eval_count;
                  if (Number.isSafeInteger(inputTokens) && inputTokens >= 0) {
                    const cache = usage.input_tokens === undefined ? 0 : (usage.cache_read_input_tokens ?? 0) + (usage.cache_creation_input_tokens ?? 0);
                    if (Number.isSafeInteger(cache) && cache >= 0) observedInput = inputTokens + cache;
                  }
                  if (Number.isSafeInteger(outputTokens) && outputTokens >= 0) {
                    const thoughts = usage.candidatesTokenCount === undefined ? 0 : usage.thoughtsTokenCount ?? 0;
                    if (Number.isSafeInteger(thoughts) && thoughts >= 0) observedOutput = outputTokens + thoughts;
                  }
                  if (typeof usage.cost === 'number' && Number.isFinite(usage.cost) && usage.cost >= 0 && Number.isSafeInteger(Math.round(usage.cost * 1_000_000))) reportedCost = Math.round(usage.cost * 1_000_000);
                }
              } catch { /* Non-JSON SSE metadata is not usage evidence. */ }
            }
            sink.enqueue(chunk);
          },
        }));
        return new Response(body, { status: response.status, statusText: response.statusText, headers: response.headers });
      };
      const options: StreamOptions = { apiKey: secret || NO_AUTH_SENTINEL, signal: controller.signal,
        maxTokens: input.max_output_tokens ?? inventory.output_limit, fetch: guardedFetch,
        streamFirstEventTimeoutMs: 1_500, streamIdleTimeoutMs: 5_000,
        acceptEmptyResponse: true, providerRetryWait: async () => { throw new Error('retry_requires_core_dispatch'); } };
      yield event({ type: 'turn_started', message_id: input.message_id });
      const stream = api === 'openai-completions' ? streamOpenAICompletions(model as Model<'openai-completions'>, context, options)
        : api === 'openai-responses' ? streamOpenAIResponses(model as Model<'openai-responses'>, context, options)
        : api === 'anthropic-messages' ? streamAnthropic(model as Model<'anthropic-messages'>, context, options)
        : api === 'google-generative-ai' ? streamGoogle(model as Model<'google-generative-ai'>, context, options)
        : streamOllama(model as Model<'ollama-chat'>, context, options);
      for await (const item of stream) {
        signal.throwIfAborted();
        // Never emit SDK error/request dumps, credential identities or SDK calculated prices.
        const partial = 'partial' in item ? item.partial : item.type === 'done' ? item.message : item.type === 'error' ? item.error : undefined;
        if (partial && (containsCredential(partial, secret) || (secret && (
          partial.content.filter(block => block.type === 'text').map(block => block.type === 'text' ? block.text : '').join('').includes(secret)
          || partial.content.filter(block => block.type === 'thinking').map(block => block.type === 'thinking' ? block.thinking : '').join('').includes(secret))))) throw new Error('credential_disclosure_denied');
        if (item.type === 'text_delta' || item.type === 'thinking_delta') {
          const key = item.type + ':' + item.contentIndex;
          const combined = (pendingText.get(key) ?? '') + item.delta;
          if (secret && combined.includes(secret)) throw new Error('credential_disclosure_denied');
          const hold = secret ? secret.length - 1 : 0;
          const end = Math.max(0, combined.length - hold);
          pendingText.set(key, combined.slice(end));
          if (end) yield event(item.type === 'text_delta' ? { type: 'text_delta', message_id: input.message_id, delta: combined.slice(0, end) }
            : { type: 'reasoning_delta', message_id: input.message_id, delta: combined.slice(0, end) });
        } else if (item.type === 'toolcall_end') {
          if (!input.tool_schemas.some(tool => tool.name === item.toolCall.name)) throw new Error('undeclared_tool_call');
          if (containsCredential(item.toolCall, secret)) throw new Error('credential_disclosure_denied');
          yield event({ type: 'provider_tool_call', message_id: input.message_id, call: { id: item.toolCall.id, name: item.toolCall.name, arguments: item.toolCall.arguments } });
        } else if (item.type === 'done' || item.type === 'error') {
          for (const [key, delta] of pendingText) if (delta) yield event(key.startsWith('text_delta')
            ? { type: 'text_delta', message_id: input.message_id, delta } : { type: 'reasoning_delta', message_id: input.message_id, delta });
          pendingText.clear();
          const message = item.type === 'done' ? item.message : item.error;
          const cost = reportedCost === undefined ? undefined : formatWireU64(BigInt(reportedCost));
          const observedUsage = observedInput !== undefined || observedOutput !== undefined;
          yield event({ type: 'usage_observed', usage: { ...(observedInput === undefined ? {} : { input_tokens: observedInput }),
            ...(observedOutput === undefined ? {} : { output_tokens: observedOutput }), source: observedUsage ? 'provider' : 'unknown',
            billing: cost === undefined ? { state: 'unknown' } : { state: 'reported', cost_microusd: cost }, ...(cost === undefined ? {} : { cost_microusd: cost }) } });
          if (item.type === 'done') {
            if (message.disabledFeatures?.length || message.content.some(block => !['text', 'thinking', 'redactedThinking', 'toolCall'].includes(block.type))) throw new Error('unsupported_provider_output');
            if (message.upstreamModel && message.upstreamModel !== inventory.id) throw new Error('pinned_model_mismatch');
            const content = { blocks: message.content,
              ...(message.providerPayload ? { provider_payload: message.providerPayload } : {}),
              ...(message.requestControls ? { request_controls: message.requestControls } : {}),
              observed_provider: message.upstreamProvider ?? null, observed_model: message.upstreamModel ?? null };
            if (containsCredential(content, secret)) throw new Error('credential_disclosure_denied');
            yield event({ type: 'provider_message', message_id: input.message_id, content, route: input.route });
          } else yield event({ type: 'error', code: signal.aborted ? 'cancelled' : 'provider_error', message: 'Pinned provider turn failed', details: { status: message.errorStatus ?? null } });
          yield event({ type: 'turn_finished', message_id: input.message_id, status: item.type === 'done' ? 'succeeded' : signal.aborted ? 'cancelled' : 'failed', stop_reason: item.reason });
          terminal = true;
        } else if (item.type === 'image_end') throw new Error('unsupported_generated_image');
      }
      if (!terminal) throw new Error('incomplete_provider_stream');
    } catch (error) {
      controller.abort();
      const boundaryCodes: Record<string, true> = { route_denied: true, local_only_denied: true, unsupported_capability: true,
        unsupported_protocol: true, unqualified_capability: true, unknown_model: true, custom_activation_required: true,
        credential_unavailable: true, credential_disclosure_denied: true, native_replay_route_mismatch: true,
        native_tool_replay_mismatch: true,
        unsupported_content: true, invalid_image: true, output_limit_denied: true, undeclared_tool_call: true,
        unsupported_provider_output: true, incomplete_provider_stream: true };
      const code = error instanceof Error && Object.hasOwn(boundaryCodes, error.message) ? error.message : 'provider_boundary_error';
      yield event({ type: 'error', code: signal.aborted ? 'cancelled' : code, message: signal.aborted ? 'Provider turn cancelled' : 'Pinned provider turn denied or failed', details: null });
      yield event({ type: 'turn_finished', message_id: input.message_id, status: signal.aborted ? 'cancelled' : 'failed' });
    } finally {
      controller.abort(); signal.removeEventListener('abort', abort);
      secretBytes?.fill(0); secret = ''; pendingText.clear();
    }
  }
}
