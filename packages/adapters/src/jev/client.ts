import { decode, object, parseCompletion, scoreEvidence } from "./decode";
import { FACADE_BASE, judgmentPrompt, runtimeBase, runtimeIdentity, sameRuntimeIdentity, sha256 } from "./identity";
import { formatWireU64 } from "../../../contracts/src/wire";
import type { JevClientOptions, JevTransport, JudgmentInput, JudgmentRecord, RuntimeIdentity } from "./types";

const UNKNOWN_RUNTIME_DIGEST = sha256("ironbark-jev:runtime-identity-unobserved:v1");
const UNKNOWN_PROMPT_DIGEST = sha256("ironbark-jev:prompt-not-constructed:v1");
const MAX_RESPONSE_BYTES = 65536;

/** Raw local evidence only: no permissions, routing, calibration or durable state. */
export class JevClient {
  private readonly transport: JevTransport;
  private readonly timeout: number;
  private readonly limit: number;
  private active = 0;
  private pinned: RuntimeIdentity | null = null;
  private invalidated = false;

  constructor(private readonly options: JevClientOptions) {
    this.transport = options.transport ?? ((url, init) => fetch(url, init));
    this.timeout = options.timeout_ms ?? 2000;
    this.limit = options.max_in_flight ?? 1;
    if (!Number.isInteger(this.timeout) || this.timeout < 1 || this.timeout > 2000)
      throw new TypeError("JEV timeout must be 1..2000 ms");
    if (!Number.isInteger(this.limit) || this.limit < 1 || this.limit > 4)
      throw new TypeError("JEV in-flight limit must be 1..4");
  }

  async judge(input: JudgmentInput, signal: AbortSignal): Promise<JudgmentRecord> {
    // Snapshot before the first await: callers cannot mutate the ongoing identity.
    const request = { ...input, candidates: [...input.candidates], input_refs: [...input.input_refs] };
    this.validate(request);
    const started = performance.now();
    const codes: Record<string, string> = Object.fromEntries(request.candidates.map((label, index) => [`C${index}`, label]));
    let promptDigest = UNKNOWN_PROMPT_DIGEST;
    let observed: RuntimeIdentity | null = null;
    const record = (state: JudgmentRecord["state"], reason?: string): JudgmentRecord => ({
      decision_id: request.decision_id, domain: request.domain, domain_version: request.domain_version,
      candidates: request.candidates, candidate_codes: codes, input_refs: request.input_refs,
      input_digest: request.input_digest, criterion: request.criterion, route_policy_digest: request.route_policy_digest,
      model_identity: observed?.model_identity ?? "unknown", runtime_identity_digest: observed?.digest ?? UNKNOWN_RUNTIME_DIGEST,
      prompt_digest: promptDigest, state, score: { type: "none" }, latency_ms: formatWireU64(BigInt(Math.ceil(performance.now() - started))),
      ...(reason ? { reason } : {}),
    });
    if (signal.aborted) return record("unavailable", "cancelled");
    if (this.active >= this.limit) return record("unavailable", "busy");
    if (this.invalidated) return record("unqualified", "runtime_identity_changed");
    this.active++;
    const controller = new AbortController();
    const onCancel = () => controller.abort("cancelled");
    signal.addEventListener("abort", onCancel, { once: true });
    const timer = setTimeout(() => controller.abort("timeout"), this.timeout);
    let onAbort: (() => void) | undefined;
    // Capture the abort reason at the moment of abort: the finally below
    // re-asserts the abort to cancel in-flight work, which would otherwise
    // overwrite the caller-facing reason before the catch reads it.
    let abortReason: unknown;
    const aborted = new Promise<never>((_, reject) => {
      onAbort = () => { abortReason = controller.signal.reason; reject(new Error("JEV aborted")); };
      controller.signal.addEventListener("abort", onAbort, { once: true });
    });
    const execute = async (): Promise<JudgmentRecord> => {
      controller.signal.throwIfAborted();
      const content = await this.options.resolveInput(request, controller.signal);
      controller.signal.throwIfAborted();
      if (typeof content !== "string" || content.length > 8192 || sha256(content) !== request.input_digest)
        return record("invalid", "input_digest_or_size_mismatch");
      const prompt = judgmentPrompt(request, content, codes);
      promptDigest = sha256(prompt);
      observed = await this.observeIdentity(controller.signal);
      if (!observed) return record("unqualified", "runtime_identity_unknown");
      if (this.pinned && !sameRuntimeIdentity(this.pinned, observed)) {
        this.invalidated = true;
        return record("unqualified", "runtime_identity_changed");
      }
      this.pinned ??= observed;
      const grammar = `root ::= (${[...Object.keys(codes), "ABSTAIN"].map(code => JSON.stringify(code)).join(" | ")})`;
      const raw = await this.request(`${observed.runtime_base}/completion`, controller.signal, {
        prompt, grammar, n_predict: 16, temperature: 0, n_probs: 10, cache_prompt: true,
        stream: false, post_sampling_probs: false,
      });
      // Probe after even an invalid completion; a runtime change takes precedence.
      const after = await this.observeIdentity(controller.signal);
      if (!sameRuntimeIdentity(observed, after)) {
        this.invalidated = true;
        return record("unqualified", after ? "runtime_identity_changed" : "runtime_identity_unknown");
      }
      const parsed = parseCompletion(raw);
      if (!parsed) return record("invalid", "invalid_completion_envelope");
      if (parsed.model !== observed.model_identity) {
        this.invalidated = true;
        return record("unqualified", "completion_model_mismatch");
      }
      const decoded = decode(parsed.content, codes);
      if (decoded.state === "invalid") return record("invalid", "invalid_exact_code");
      const result = record(decoded.state);
      if (decoded.state === "available") result.choice = decoded.choice;
      result.score = scoreEvidence(parsed);
      return result;
    };
    try {
      return await Promise.race([execute(), aborted]);
    } catch {
      // Never copy runtime errors, raw text, credentials or paths into global logs.
      // A synchronous caller abort during request() can surface the raw
      // AbortError rather than our string reason; both mean cancelled.
      return record("unavailable", abortReason === "timeout" ? "timeout"
        : abortReason === "cancelled" || (controller.signal.reason as { name?: string } | undefined)?.name === "AbortError" ? "cancelled"
        : "service_unavailable");
    } finally {
      // Abort on every exit (including early probe failure) so in-flight sibling
      // probes/readers are cancelled instead of running on without a deadline.
      // A noncooperative injected transport cannot be forcibly stopped; the slot
      // is freed only here, after the operation's own work has settled.
      clearTimeout(timer);
      controller.abort();
      signal.removeEventListener("abort", onCancel);
      if (onAbort) controller.signal.removeEventListener("abort", onAbort);
      this.active--;
    }
  }

  private validate(input: JudgmentInput): void {
    if (typeof input.decision_id !== "string" || !input.decision_id ||
        typeof input.domain !== "string" || !input.domain || input.domain.length > 128 ||
        typeof input.domain_version !== "string" || !input.domain_version || input.domain_version.length > 128 ||
        typeof input.criterion !== "string" || !input.criterion || input.criterion.length > 2048 ||
        !Array.isArray(input.candidates) || input.candidates.length < 1 || input.candidates.length > 32 ||
        input.candidates.some(label => typeof label !== "string" || !label || label === "ABSTAIN" || label.length > 128) ||
        new Set(input.candidates).size !== input.candidates.length ||
        !Array.isArray(input.input_refs) || input.input_refs.length > 64 ||
        input.input_refs.some(ref => typeof ref !== "string" || !ref || ref.length > 256) ||
        !/^[0-9a-f]{64}$/.test(input.input_digest) || !/^[0-9a-f]{64}$/.test(input.route_policy_digest))
      throw new TypeError("Invalid JEV judgment input");
  }

  private async request(url: string, signal: AbortSignal, body?: Record<string, unknown>): Promise<string> {
    signal.throwIfAborted();
    const response = await this.transport(url, { method: body ? "POST" : "GET", redirect: "error", signal,
      headers: { "Accept": "application/json", ...(body ? { "Content-Type": "application/json" } : {}) },
      ...(body ? { body: JSON.stringify(body) } : {}) });
    signal.throwIfAborted();
    if (!response.ok || response.redirected || (response.url && response.url !== url) ||
        !response.headers.get("content-type")?.toLowerCase().startsWith("application/json"))
      throw new Error("Invalid JEV HTTP response");
    if (!response.body) throw new Error("Empty JEV HTTP response");
    const reader = response.body.getReader();
    const chunks: Uint8Array[] = [];
    let size = 0;
    try {
      while (true) {
        signal.throwIfAborted();
        const chunk = await reader.read();
        signal.throwIfAborted();
        if (chunk.done) break;
        size += chunk.value.byteLength;
        if (size > MAX_RESPONSE_BYTES) throw new Error("Oversized JEV HTTP response");
        chunks.push(chunk.value);
      }
    } finally {
      void reader.cancel().catch(() => {});
      reader.releaseLock();
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  }

  async observeIdentity(signal: AbortSignal): Promise<RuntimeIdentity | null> {
    // Fast-fail: the first probe request failure rejects as soon as it is
    // observed. An operation abort (timeout, cancellation, or the caller's
    // finally after an early failure) rejects cooperative transports via their
    // signal and the checkpoints inside request(), so sibling probes are
    // cancelled instead of running on without a deadline. A noncooperative
    // injected transport cannot be stopped; the operation result and slot do
    // not wait for it.
    try {
      const facade: unknown = JSON.parse(await this.request(`${FACADE_BASE}/api/status`, signal));
      const base = runtimeBase(facade);
      if (!base) return null;
      const values = await Promise.all([
        this.request(`${base}/health`, signal),
        this.request(`${base}/props`, signal),
      ]);
      const parsed: unknown[] = values.map(value => JSON.parse(value));
      if (!parsed.every(object)) return null;
      return runtimeIdentity(facade, parsed[0], parsed[1]);
    } catch {
      signal.throwIfAborted();
      throw new Error("probe_failed");
    }
  }
}
