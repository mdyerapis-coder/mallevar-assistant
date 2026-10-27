import { describe, expect, test } from "bun:test";
import { JevClient } from "../src/jev/client";
import { decode, decodeCompletion, scoreEvidence } from "../src/jev/decode";
import { sha256, runtimeIdentity, sameRuntimeIdentity } from "../src/jev/identity";
import type { JudgmentInput, JevTransport } from "../src/jev/types";

// Each negative fixture catches a consumer bug: coercion, confidence invention,
// trusting a JSON assertion, lost provenance, unsafe transport, or stale identity.
const candidates = ["REVIEW", "RESEARCH"];
const codes = { C0: "REVIEW", C1: "RESEARCH" };
const text = "Synthetic task: review a small patch.";
function input(overrides: Partial<JudgmentInput> = {}): JudgmentInput {
  return { decision_id: "00000000-0000-4000-8000-000000000001", domain: "intent", domain_version: "1",
    candidates: [...candidates], input_refs: ["synthetic:task"], input_digest: sha256(text),
    criterion: "Choose the task intent; abstain if ambiguous.", route_policy_digest: "a".repeat(64), ...overrides };
}
const facade = { llm: { online: true, mode: "local", port: 8091,
  active_model_id: "Qwen2.5-0.5B-Instruct-Q8_0.gguf", active_model_path: "/models/Qwen2.5-0.5B-Instruct-Q8_0.gguf",
  base_url: "http://127.0.0.1:8091/v1", device: "CPU" }, offline_only: true };
const props = { model_alias: "Qwen2.5-0.5B-Instruct-Q8_0.gguf", model_path: "/models/Qwen2.5-0.5B-Instruct-Q8_0.gguf",
  model_ftype: 7, build_info: "b11559-fixture", chat_template: "fixture-template", total_slots: 1,
  default_generation_settings: { n_ctx: 4096, params: { temperature: 0.8, lora: [] } }, is_sleeping: false };
function completion(overrides: Record<string, unknown> = {}) {
  return { content: "C0", stop: true, truncated: false, stop_type: "eos", model: props.model_alias,
    tokens_predicted: 3, tokens_evaluated: 100, ...overrides };
}
function fixture(options: { body?: unknown; changeIdentity?: boolean; fail?: boolean } = {}) {
  const requests: { url: string; init: RequestInit }[] = [];
  let probes = 0;
  const transport: JevTransport = async (url, init) => {
    requests.push({ url, init });
    if (options.fail) throw new Error("fixture offline");
    if (url.endsWith("/api/status")) return Response.json(facade);
    if (url.endsWith("/health")) return Response.json({ status: "ok" });
    if (url.endsWith("/props")) {
      probes++;
      return Response.json(options.changeIdentity && probes > 1 ? { ...props, build_info: "changed" } : props);
    }
    if (url.endsWith("/completion")) return Response.json(options.body ?? completion());
    throw new Error("Unexpected endpoint");
  };
  return { transport, requests };
}
function client(transport: JevTransport, extra: { timeout_ms?: number } = {}) {
  return new JevClient({ transport, resolveInput: async () => text, ...extra });
}

describe("strict raw output", () => {
  test("prefix_output_is_invalid", () => expect(decode("REV", candidates)).toEqual({ state: "invalid" }));
  test("empty_choice_is_not_first_candidate", () => {
    const emptyDecoded = decode("", candidates);
    expect(emptyDecoded).toEqual({ state: "invalid" });
    expect("choice" in emptyDecoded ? emptyDecoded.choice : undefined).toBeUndefined();
  });
  test("exact_codes_map_to_labels_and_abstain_is_explicit", () => {
    expect(decode("C1", codes)).toEqual({ state: "available", choice: "RESEARCH" });
    expect(decode("ABSTAIN", codes)).toEqual({ state: "abstain" });
  });
  test("no_case_whitespace_json_or_trailing_text_coercion", () => {
    for (const value of [" C0", "C0\n", "c0", '"C0"', '{"choice":"C0"}', "C0 extra", "C", "C00", "REVIEW"])
      expect(decode(value, codes)).toEqual({ state: "invalid" });
  });
  test("prototype_keys_do_not_become_candidates", () => expect(decode("toString", codes)).toEqual({ state: "invalid" }));
  test("malformed_json_and_wrong_field_types_are_invalid", () => {
    for (const value of ['{', 'null', '[]', '"C0"', '{"content":0}', '{"content":{"toString":"C0"}}'])
      expect(decodeCompletion(value, codes).state).toBe("invalid");
  });
  test("generation_limits_and_prompt_truncation_are_invalid_even_with_exact_content", () => {
    for (const overrides of [{ stop: false }, { truncated: true }, { stop_type: "limit" }, { stop_type: "none" }])
      expect(decodeCompletion(JSON.stringify(completion(overrides)), codes).state).toBe("invalid");
    expect(decodeCompletion(JSON.stringify(completion()), codes).state).toBe("available");
  });
  test("missing_probability_is_unknown", () => expect(scoreEvidence(completion())).toEqual({ type: "none" }));
  test("shared_prefix_score_is_not_label_confidence", () => {
    const score = scoreEvidence(completion({ content: "C1", completion_probabilities: [{ token: "C", logprob: -0.7,
      top_logprobs: [{ token: "C", logprob: -0.7 }] }] }));
    expect(score).toEqual({ type: "raw_token_log_prob", value: -0.7, token: "C", token_position: 0 });
    expect("confidence" in score).toBe(false);
    expect("upper_error_bound" in score).toBe(false);
  });
  test("nonfinite_positive_string_and_mismatched_probabilities_are_unknown", () => {
    for (const entry of [{ token: "C", logprob: "-0.2" }, { token: "C", logprob: 0.2 },
      { token: "C", logprob: NaN }, { token: "wrong", logprob: -0.2 }, { token: "", logprob: -0.2 },
      { token: "C", prob: 0.9 }, { logprob: -0.2 }])
      expect(scoreEvidence(completion({ completion_probabilities: [entry] }))).toEqual({ type: "none" });
  });
});

describe("runtime and provenance boundary", () => {
  test("runtime_identity_change_invalidates_qualification", () => {
    const first = runtimeIdentity(facade, { status: "ok" }, props);
    const changed = runtimeIdentity(facade, { status: "ok" }, { ...props, build_info: "other" });
    expect(first).not.toBeNull();
    expect(sameRuntimeIdentity(first, changed)).toBe(false);
    expect(sameRuntimeIdentity(first, null)).toBe(false);
  });
  test("unreported_or_inconsistent_model_state_is_unknown", () => {
    for (const p of [{ ...props, model_path: "/models/other.gguf" }, { ...props, build_info: "" }, { ...props, is_sleeping: true }])
      expect(runtimeIdentity(facade, { status: "ok" }, p)).toBeNull();
    expect(runtimeIdentity({ llm: { ...facade.llm, mode: "openai" } }, { status: "ok" }, props)).toBeNull();
  });
  test("observes_native_string_quantization_metadata_without_accepting_empty_identity", () => {
    expect(runtimeIdentity(facade, { status: "ok" }, { ...props, model_ftype: "Q8_0" })?.model_identity)
      .toBe(props.model_alias);
    for (const model_ftype of ["", null, {}, 1.5]) {
      expect(runtimeIdentity(facade, { status: "ok" }, { ...props, model_ftype })).toBeNull();
    }
  });
  test("uses_the_facades_verified_local_runtime_without_contacting_the_stale_port", async () => {
    const active = { llm: { ...facade.llm, port: 8092, base_url: "http://127.0.0.1:8092/v1" } };
    const requests: string[] = [];
    const transport: JevTransport = async (url) => {
      requests.push(url);
      if (url.endsWith("/api/status")) return Response.json(active);
      if (!url.startsWith("http://127.0.0.1:8092/")) throw new Error("stale runtime");
      if (url.endsWith("/health")) return Response.json({ status: "ok" });
      if (url.endsWith("/props")) return Response.json(props);
      return Response.json(completion());
    };
    const record = await client(transport).judge(input(), new AbortController().signal);
    expect(record.state).toBe("available");
    expect(record.choice).toBe("REVIEW");
    expect(requests).toContain("http://127.0.0.1:8092/completion");
    expect(requests.some(url => url.startsWith("http://127.0.0.1:8091/"))).toBe(false);
  });
  test("facade_cannot_redirect_judgments_to_a_remote_or_inconsistent_runtime", async () => {
    for (const base_url of ["https://example.com/v1", "http://127.0.0.1:8092/v1", "http://user:pass@127.0.0.1:8091/v1"]) {
      let downstream = 0;
      const transport: JevTransport = async (url) => {
        if (url.endsWith("/api/status")) return Response.json({ llm: { ...facade.llm, base_url } });
        downstream++;
        throw new Error("must not disclose to an unverified runtime");
      };
      const record = await client(transport).judge(input(), new AbortController().signal);
      expect(record.state).toBe("unqualified");
      expect(downstream).toBe(0);
    }
  });
  test("metadata_object_key_order_does_not_change_identity", () => {
    expect(runtimeIdentity(facade, { status: "ok" }, { ...props,
      default_generation_settings: { params: { lora: [], temperature: 0.8 }, n_ctx: 4096 } })?.digest)
      .toBe(runtimeIdentity(facade, { status: "ok" }, props)?.digest);
  });
  test("valid_raw_record_retains_identity_mapping_and_unknown_qualification", async () => {
    const f = fixture();
    const record = await client(f.transport).judge(input(), new AbortController().signal);
    expect(record.state).toBe("available");
    expect(record.choice).toBe("REVIEW");
    expect(record.candidate_codes).toEqual(codes);
    expect(record.input_refs).toEqual(["synthetic:task"]);
    expect(record.input_digest).toBe(sha256(text));
    expect(record.score).toEqual({ type: "none" });
    expect(record.qualification_digest).toBeUndefined();
    expect(record.prompt_digest).toMatch(/^[0-9a-f]{64}$/);
    // Wire contract: latency_ms is a canonical uint64 decimal STRING
    // (Rust serde wire::u64; generated JudgmentRecord type), never a JSON
    // number. Assert the canonical lexeme, not a numeric type.
    expect(record.latency_ms).toMatch(/^(0|[1-9][0-9]*)$/);
    expect(BigInt(record.latency_ms)).toBeGreaterThanOrEqual(0n);
    const body = JSON.parse(f.requests.find(r => r.url.endsWith("/completion"))!.init.body as string);
    expect(body.grammar).toContain('"ABSTAIN"');
    expect(body.prompt).toContain(text);
    expect(body.n_probs).toBe(10);
    expect(body.stream).toBe(false);
    expect(f.requests.every(r => r.url.startsWith("http://127.0.0.1:"))).toBe(true);
    expect(f.requests.every(r => r.init.redirect === "error")).toBe(true);
    expect(JSON.stringify(record)).not.toContain(text);
  });
  test("identity_change_during_completion_discards_choice_and_stays_invalidated", async () => {
    const f = fixture({ changeIdentity: true });
    const c = client(f.transport);
    for (let i = 0; i < 2; i++) {
      const record = await c.judge(input(), new AbortController().signal);
      expect(record.state).toBe("unqualified");
      expect(record.choice).toBeUndefined();
    }
    expect(f.requests.filter(r => r.url.endsWith("/completion"))).toHaveLength(1);
  });
  test("completion_model_mismatch_discards_choice", async () => {
    const f = fixture({ body: completion({ model: "other" }) });
    expect((await client(f.transport).judge(input(), new AbortController().signal)).state).toBe("unqualified");
  });
  test("input_digest_mismatch_never_reaches_completion", async () => {
    const f = fixture();
    const record = await client(f.transport).judge(input({ input_digest: "b".repeat(64) }), new AbortController().signal);
    expect(record.state).toBe("invalid");
    expect(record.choice).toBeUndefined();
    expect(f.requests).toHaveLength(0);
  });
  test("input_mutation_cannot_change_an_inflight_decision", async () => {
    const f = fixture();
    const original = input();
    const pending = client(f.transport).judge(original, new AbortController().signal);
    original.candidates[0] = "MUTATED";
    original.input_refs.push("private");
    const record = await pending;
    expect(record.candidates).toEqual(candidates);
    expect(record.input_refs).toEqual(["synthetic:task"]);
  });
  test("outage_is_unavailable_without_fallback", async () => {
    const f = fixture({ fail: true });
    const record = await client(f.transport).judge(input(), new AbortController().signal);
    expect(record.state).toBe("unavailable");
    expect(record.choice).toBeUndefined();
    expect(record.score).toEqual({ type: "none" });
    expect(f.requests.every(r => !r.url.endsWith("/completion"))).toBe(true);
  });
  test("cancelled_before_dispatch_makes_no_request", async () => {
    const f = fixture();
    const abort = new AbortController(); abort.abort();
    const record = await client(f.transport).judge(input(), abort.signal);
    expect(record.state).toBe("unavailable");
    expect(f.requests).toHaveLength(0);
  });
  test("deadline_bounds_noncooperative_transport_and_concurrent_requests_are_refused", async () => {
    const c = client(async () => new Promise<Response>(() => {}), { timeout_ms: 20 });
    const first = c.judge(input(), new AbortController().signal);
    const busy = await c.judge(input(), new AbortController().signal);
    expect(busy.state).toBe("unavailable");
    expect(busy.reason).toBe("busy");
    const timed = await first;
    expect(timed.state).toBe("unavailable");
    expect(timed.reason).toBe("timeout");
  });
  test("invalid_raw_fixture_does_not_publish_choice_or_score", async () => {
    const f = fixture({ body: completion({ content: "C" }) });
    const record = await client(f.transport).judge(input(), new AbortController().signal);
    expect(record.state).toBe("invalid");
    expect(record.choice).toBeUndefined();
    expect(record.score).toEqual({ type: "none" });
  });
  test("prompt_identity_binds_domain_version_criterion_candidates_and_policy", async () => {
    const f = fixture(); const c = client(f.transport);
    const base = await c.judge(input(), new AbortController().signal);
    for (const change of [{ domain_version: "2" }, { criterion: "Other criterion" }, { candidates: ["RESEARCH", "REVIEW"] },
      { route_policy_digest: "c".repeat(64) }]) {
      const result = await c.judge(input(change), new AbortController().signal);
      expect(result.prompt_digest).not.toBe(base.prompt_digest);
    }
  });
});

describe("transport and cancellation safety", () => {
  test("inflight_cancellation_never_becomes_a_choice", async () => {
    const abort = new AbortController();
    const c = client(async () => {
      abort.abort();
      return new Promise<Response>(() => {});
    });
    const record = await c.judge(input(), abort.signal);
    expect(record.state).toBe("unavailable");
    expect(record.reason).toBe("cancelled");
    expect(record.choice).toBeUndefined();
  });
  test("redirect_and_non_json_responses_are_not_followed_or_trusted", async () => {
    for (const response of [new Response("", { status: 302, headers: { location: "https://example.invalid/" } }),
      new Response('{"status":"ok"}', { headers: { "content-type": "text/html" } })]) {
      const record = await client(async () => response).judge(input(), new AbortController().signal);
      expect(record.state).toBe("unavailable");
      expect(record.choice).toBeUndefined();
    }
  });
  test("oversized_responses_are_bounded_and_never_recorded", async () => {
    const record = await client(async () => new Response("x".repeat(65537),
      { headers: { "content-type": "application/json" } })).judge(input(), new AbortController().signal);
    expect(record.state).toBe("unavailable");
    expect(record.choice).toBeUndefined();
    expect(JSON.stringify(record).length).toBeLessThan(4096);
  });
  test("malformed_completion_json_is_invalid_not_unavailable_or_a_default", async () => {
    const f = fixture();
    const transport: JevTransport = (url, init) => url.endsWith("/completion")
      ? Promise.resolve(new Response("{", { headers: { "content-type": "application/json" } }))
      : f.transport(url, init);
    const record = await client(transport).judge(input(), new AbortController().signal);
    expect(record.state).toBe("invalid");
    expect(record.choice).toBeUndefined();
  });
  test("timeout_cannot_be_extended_and_duplicate_labels_are_rejected", async () => {
    expect(() => new JevClient({ resolveInput: async () => text, timeout_ms: 2001 })).toThrow();
    const f = fixture();
    await expect(client(f.transport).judge(input({ candidates: ["REVIEW", "REVIEW"] }),
      new AbortController().signal)).rejects.toThrow();
    expect(f.requests).toHaveLength(0);
  });
  test("early_probe_failure_aborts_stalled_sibling_and_frees_slot_without_second_request", async () => {
    const f = fixture();
    const signals: AbortSignal[] = [];
    const urls: string[] = [];
    const transport: JevTransport = async (url, init) => {
      signals.push(init.signal!);
      urls.push(url);
      if (url.endsWith("/api/status")) return new Response("boom", { status: 500 });
      if (url.endsWith("/health") || url.endsWith("/props")) {
        // Real fetch rejects when its signal aborts; model that so the sibling
        // settles as soon as the operation is torn down.
        await new Promise<void>((resolve, reject) => init.signal!.addEventListener("abort", () => { reject(init.signal!.reason); }, { once: true }));
        return Response.json({ status: "ok" });
      }
      return f.transport(url, init);
    };
    const record = await client(transport).judge(input(), new AbortController().signal);
    expect(record.state).toBe("unavailable");
    expect(record.reason).toBe("service_unavailable");
    expect(record.choice).toBeUndefined();
    // The operation controller was aborted on exit, cancelling the stalled siblings.
    for (const signal of signals) expect(signal.aborted).toBe(true);
    // The slot was freed: a second judgment starts and no completion request was
    // issued by either operation.
    const second = await client(transport).judge(input(), new AbortController().signal);
    expect(second.state).toBe("unavailable");
    expect(second.reason).toBe("service_unavailable");
    expect(urls.some(url => url.endsWith("/completion"))).toBe(false);
  });
});
