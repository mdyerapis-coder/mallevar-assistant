//! Provider round-trip smoke: drives the real worker entry point
//! (`serveProviderRpc`, the same framing `src/main.ts` uses) and the real
//! `OmpProviderAdapter` against a live local llama.cpp OpenAI-compatible
//! endpoint on 127.0.0.1 (default port 8091, override with `--port`).
//!
//! Proves, with recorded observations rather than assumptions:
//!   1. a real streamed local answer (actual text, effective route, stop reason);
//!   2. the effective route is byte-identical to the requested route and no
//!      second provider/connection is contacted (exactly one pinned request);
//!   3. an abort mid-stream ends as cancelled, never as a completed success;
//!   4. usage/cost is recorded exactly as the adapter produced it, including the
//!      honest `unknown` state when the wire carries no billing evidence;
//!   5. a negative control (closed loopback port) fails as a typed
//!      unavailable/transport error and never quietly succeeds or reroutes.
//!
//! Only loopback is contacted. No paid/private-context probes. No operator
//! install or running daemon is involved: the only endpoints are the loopback
//! URLs named below.
import { strict as assert } from "node:assert";
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import type {
  ConnectionConfig,
  ModelDescriptor,
  ProjectPolicy,
  RoutePin,
  WorkerEvent,
  WorkerProviderTurnInput,
} from "../../packages/contracts/src/generated.ts";
import { OmpProviderAdapter } from "../../packages/adapters/src/providers/omp-sdk.ts";
import { encodeFrame, serveProviderRpc } from "../../packages/adapters/src/rpc.ts";

const ROOT = resolve(import.meta.dir, "../..");
const COMMAND = "bun qa/smoke/provider-roundtrip.ts";

const args = process.argv.slice(2);
const portFlag = args.indexOf("--port");
const port = Number(portFlag < 0 ? (process.env.IRONBARK_LLAMA_PORT ?? 8091) : args[portFlag + 1]);
assert(Number.isInteger(port) && port > 0 && port < 65536, `invalid port: ${port}`);
const endpoint = `http://127.0.0.1:${port}`;
// A loopback port with nothing listening: connection refused, no retry.
const CLOSED_ENDPOINT = "http://127.0.0.1:1";
const PROJECT = "provider-roundtrip-project";
const CUSTOM_IMPLEMENTATION_VERSION = "18.8.9"; // the SDK version the registry compares against

const receiptPath = resolve(
  ROOT,
  process.env.IRONBARK_EVIDENCE_FILE ?? ".superpowers/sdd/2026-10-12-ironbark-assistant/qa-smoke-provider-roundtrip-ts.json",
);

const sha256 = (bytes: Uint8Array | string): string => createHash("sha256").update(bytes).digest("hex");
const message = (error: unknown): string => (error instanceof Error ? error.message : String(error));

/** A decoded RPC frame: the worker writes exactly one JSON object per line. */
type Frame = Record<string, unknown>;
function frameOf(value: unknown): Frame {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("rpc_frame_not_object");
  return value as Frame; // any JSON object is a Record<string, unknown>
}
/** The `result` object of a reply frame; empty when the frame carries none. */
function resultOf(frame: Frame): Record<string, unknown> {
  const result = frame.result;
  return typeof result === "object" && result !== null && !Array.isArray(result)
    ? (result as Record<string, unknown>)
    : {};
}
/** A decoded `event` frame's WorkerEvent; undefined when the frame is not one. */
function workerEventOf(value: unknown): WorkerEvent | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
  const event = value as WorkerEvent; // produced by serveProviderRpc from a serialized WorkerEvent
  if (typeof event.run_id !== "string" || typeof event.payload !== "object" || event.payload === null) return undefined;
  return typeof event.payload.type === "string" ? event : undefined;
}

// ── fetch accounting: every provider HTTP call is observed, never guessed ─────
// The adapter (through the SDK) always dispatches via the global fetch binding,
// so wrapping it is a real, complete record of outbound requests. No other
// transport exists in the worker.
const realFetch = globalThis.fetch;
const requests: { method: string; url: string; at: number }[] = [];
// The wrapper preserves the exact (input, init) contract; the cast only bridges
// the DOM/undici overload set, which no single-function signature unifies with.
globalThis.fetch = ((async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
  const method = init?.method ?? (input instanceof Request ? input.method : "GET");
  requests.push({ method, url, at: Date.now() });
  return realFetch(input, init);
}) as typeof fetch);

const evidence: Record<string, unknown> = {
  mechanism: "real worker framing (serveProviderRpc) + real OmpProviderAdapter against a live loopback llama.cpp endpoint",
  simulated_model_success: false,
  endpoint,
};
const checks: { name: string; ok: boolean; detail?: string }[] = [];
function expect(name: string, condition: boolean, detail?: string) {
  checks.push({ name, ok: condition, ...(detail ? { detail } : {}) });
  if (!condition) throw new Error(`assertion_failed: ${name}${detail ? ` — ${detail}` : ""}`);
}

// ── RPC channel: bytes in, frames out, matching on request_id ────────────────
const decoder = new TextDecoder();
function createSink() {
  const frames: Frame[] = [];
  const waiters = new Set<() => void>();
  const write = async (bytes: Uint8Array) => {
    for (const line of decoder.decode(bytes).split("\n")) {
      const trimmed = line.trim();
      if (trimmed) frames.push(frameOf(JSON.parse(trimmed)));
    }
    for (const wake of [...waiters]) wake();
  };
  function next(predicate: (frame: Frame) => boolean, timeoutMs = 30_000): Promise<Frame> {
    return new Promise<Frame>((resolveFrame, rejectFrame) => {
      let timer: ReturnType<typeof setTimeout>;
      const attempt = () => {
        const index = frames.findIndex(predicate);
        if (index < 0) return;
        const [frame] = frames.splice(index, 1);
        waiters.delete(attempt);
        clearTimeout(timer);
        resolveFrame(frame);
      };
      timer = setTimeout(() => {
        waiters.delete(attempt);
        rejectFrame(new Error("rpc_frame_timeout"));
      }, timeoutMs);
      waiters.add(attempt);
      attempt();
    });
  }
  return { write, next };
}

let inputController!: ReadableStreamDefaultController<Uint8Array>;
const input = new ReadableStream<Uint8Array>({ start(controller) { inputController = controller; } });
async function* framesOf(stream: ReadableStream<Uint8Array>): AsyncGenerator<Uint8Array> {
  const reader = stream.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      yield value;
    }
  } finally {
    reader.releaseLock();
  }
}
const send = (frame: unknown) => inputController.enqueue(encodeFrame(frame));

// Local loopback is keyless: the resolver yields no credential rather than a
// fabricated one. The adapter permits an empty secret only for literal loopback.
const credentials = async (_leaseId: string, signal: AbortSignal): Promise<Uint8Array> => {
  signal.throwIfAborted();
  return new Uint8Array(0);
};

const sink = createSink();
const worker = serveProviderRpc(framesOf(input), sink.write, (connection: ConnectionConfig) =>
  new OmpProviderAdapter([connection], credentials));

function policy(connectionId: string): ProjectPolicy {
  return {
    version: "1",
    project_id: PROJECT,
    allowed_connection_ids: [connectionId],
    allowed_tools: [],
    read_roots: [],
    write_roots: [],
    network_destinations: ["127.0.0.1"],
    local_only: true,
    max_workers: 1,
    max_requests: 1,
    spend_policy: "free_only",
    allow_unknown_cost: true,
  };
}

function turn(connection: ConnectionConfig, requestId: string, prompt: string, maxOutput: number): WorkerProviderTurnInput {
  const route: RoutePin = { connection_id: connection.id, model_id: connection.models[0]!.id };
  return {
    credential_lease_id: "local-keyless-lease",
    connection,
    run_id: `run-${requestId}`,
    request_id: requestId,
    message_id: `msg-${requestId}`,
    route,
    messages: [{ role: "user", content: [{ type: "text", text: prompt }] }],
    required_capabilities: [],
    tool_schemas: [],
    disclosure_digest: "0".repeat(64),
    policy: policy(connection.id),
    max_output_tokens: maxOutput,
  };
}

/** Collect every event for a turn until its terminal reply/error frame. */
async function drive(requestId: string): Promise<{ events: WorkerEvent[]; terminal: Frame }> {
  const events: WorkerEvent[] = [];
  for (;;) {
    const frame = await sink.next(
      (f) => f.request_id === requestId && (f.type === "event" || f.type === "reply" || f.type === "error"),
    );
    if (frame.type === "event") {
      const event = workerEventOf(frame.event);
      if (!event) throw new Error("rpc_event_not_worker_event");
      events.push(event);
      continue;
    }
    return { events, terminal: frame };
  }
}

function summarize(events: WorkerEvent[]): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const event of events) counts[event.payload.type] = (counts[event.payload.type] ?? 0) + 1;
  return counts;
}
function textOf(events: WorkerEvent[]): string {
  let text = "";
  for (const event of events) if (event.payload.type === "text_delta") text += event.payload.delta;
  return text;
}

let state = "running";
try {
  // ── 1. Real capability probe: what does the endpoint actually report? ──────
  const modelsResponse = await fetch(`${endpoint}/v1/models`);
  const modelsBody = await modelsResponse.text();
  const probeDigest = sha256(modelsBody);
  const parsedModels: unknown = JSON.parse(modelsBody);
  const data = typeof parsedModels === "object" && parsedModels !== null && "data" in parsedModels
    ? parsedModels.data
    : undefined;
  const reportedIds: string[] = [];
  if (Array.isArray(data)) {
    for (const entry of data) {
      if (typeof entry === "object" && entry !== null && "id" in entry && typeof entry.id === "string") {
        reportedIds.push(entry.id);
      }
    }
  }
  expect("probe_http_200", modelsResponse.ok, `status ${modelsResponse.status}`);
  expect("probe_reports_at_least_one_model", reportedIds.length >= 1, JSON.stringify(reportedIds));
  expect("probe_reports_model_id_strings", reportedIds.every((id) => id.length > 0));
  const modelId = reportedIds[0]!;
  evidence.probe = {
    url: `${endpoint}/v1/models`,
    http_status: modelsResponse.status,
    reported_model_ids: reportedIds,
    evidence_digest: probeDigest,
    note: "llama.cpp /v1/models reports model identities but no capability metadata; capabilities below are declared by policy against the reported id.",
  };

  // The registered connection is derived from the probe, not from a fixture.
  const model: ModelDescriptor = {
    id: modelId,
    context_limit: 32_768,
    output_limit: 4_096,
    capabilities: [
      {
        name: "text",
        state: "qualified",
        implementation_version: CUSTOM_IMPLEMENTATION_VERSION,
        evidence_digest: probeDigest,
        observed_at: String(Date.now()),
      },
    ],
  };
  const connection: ConnectionConfig = {
    id: "local-llama-cpp",
    protocol: "llama_cpp",
    endpoint,
    models: [model],
    allowed_projects: [PROJECT],
  };

  // ── RPC hello + describe (the worker's own capability descriptor) ──────────
  send({ protocol: 1, request_id: "hello", method: "hello" });
  const hello = await sink.next((f) => f.type === "reply" && f.request_id === "hello");
  expect("hello_worker_is_provider", resultOf(hello).worker === "provider", JSON.stringify(hello.result));

  send({ protocol: 1, request_id: "describe", method: "describe", connection });
  const described = await sink.next((f) => f.type === "reply" && f.request_id === "describe");
  const descriptor = resultOf(described);
  expect("describe_protocol_llama_cpp", descriptor.protocol === "llama_cpp", JSON.stringify(descriptor.protocol));
  expect("describe_endpoint_matches", descriptor.endpoint === endpoint, JSON.stringify(descriptor.endpoint));
  evidence.describe = descriptor;

  // ── 2. One real streamed turn ─────────────────────────────────────────────
  const turn1Id = crypto.randomUUID();
  const prompt1 = "Reply with exactly this text and nothing else: IRONBARK LOCAL ROUNDTRIP OK";
  const input1 = turn(connection, turn1Id, prompt1, 64);
  const turn1Start = requests.length;
  const turn1Begin = Date.now();
  send({ protocol: 1, request_id: turn1Id, method: "stream", input: input1 });
  const { events: events1, terminal: terminal1 } = await drive(turn1Id);
  const turn1Ms = Date.now() - turn1Begin;
  const turn1Requests = requests.slice(turn1Start);

  const text1 = textOf(events1);
  const providerMessage1 = events1.find((e) => e.payload.type === "provider_message");
  const usage1 = events1.find((e) => e.payload.type === "usage_observed");
  const finished1 = events1.find((e) => e.payload.type === "turn_finished");

  // Effective route must be byte-identical to the requested route.
  let reportedRoute: unknown = null;
  let observedModel: string | null = null;
  if (providerMessage1 && providerMessage1.payload.type === "provider_message") {
    reportedRoute = providerMessage1.payload.route;
    const content = providerMessage1.payload.content;
    if (typeof content === "object" && content !== null && !Array.isArray(content)
      && "observed_model" in content && typeof content.observed_model === "string") {
      observedModel = content.observed_model;
    }
  }
  const requestedRoute = JSON.stringify(input1.route);

  evidence.turn = {
    request_id: turn1Id,
    prompt: prompt1,
    response_text: text1,
    effective_route: reportedRoute,
    requested_route: input1.route,
    observed_model: observedModel,
    stop_reason: finished1?.payload.type === "turn_finished" ? finished1.payload.stop_reason : null,
    status: finished1?.payload.type === "turn_finished" ? finished1.payload.status : null,
    usage: usage1?.payload.type === "usage_observed" ? usage1.payload.usage : null,
    event_counts: summarize(events1),
    error_event: events1.find((e) => e.payload.type === "error")?.payload ?? null,
    timings: { total_ms: turn1Ms },
    terminal: { type: terminal1.type, result: terminal1.result ?? terminal1.code ?? null },
    requests: turn1Requests,
  };

  expect("turn1_terminal_reply_succeeded", terminal1.type === "reply" && resultOf(terminal1).status === "succeeded",
    `${terminal1.type} ${JSON.stringify(terminal1.result ?? terminal1.code)}`);
  expect("turn1_finished_succeeded", finished1?.payload.type === "turn_finished" && finished1.payload.status === "succeeded");
  expect("turn1_real_streamed_text", text1.length > 0, JSON.stringify(text1));
  expect("turn1_no_error_event", !events1.some((e) => e.payload.type === "error"));

  expect("no_silent_fallback_route_bytes_identical", JSON.stringify(reportedRoute) === requestedRoute,
    `requested ${requestedRoute} reported ${JSON.stringify(reportedRoute)}`);
  expect("no_silent_fallback_observed_model", observedModel === null || observedModel === modelId, JSON.stringify(observedModel));

  // Exactly one request, pinned to the registered origin and chat path.
  expect("turn1_exactly_one_request", turn1Requests.length === 1, JSON.stringify(turn1Requests));
  const onlyRequest = turn1Requests[0];
  expect("turn1_request_is_pinned_post",
    onlyRequest !== undefined && onlyRequest.method === "POST" && onlyRequest.url === `${endpoint}/chat/completions`,
    JSON.stringify(onlyRequest));
  expect("turn1_no_second_connection_contacted",
    turn1Requests.every((request) => request.url.startsWith(`${endpoint}/`)),
    JSON.stringify(turn1Requests));

  // ── 3. Abort mid-stream ends cancelled, never succeeded ───────────────────
  const turn2Id = crypto.randomUUID();
  const prompt2 = "Write the integers from 1 to 400, one per line, with no other text.";
  const input2 = turn(connection, turn2Id, prompt2, 2_048);
  const turn2Start = requests.length;
  send({ protocol: 1, request_id: turn2Id, method: "stream", input: input2 });
  const firstEvent = await sink.next(
    (f) => f.type === "event" && f.request_id === turn2Id && workerEventOf(f.event)?.payload.type === "text_delta",
    15_000,
  );
  const firstWorkerEvent = workerEventOf(firstEvent.event);
  if (!firstWorkerEvent || firstWorkerEvent.payload.type !== "text_delta") {
    throw new Error("abort_first_event_not_text_delta");
  }
  const partialBeforeAbort = firstWorkerEvent.payload.delta;
  const cancelId = crypto.randomUUID();
  send({ protocol: 1, request_id: cancelId, method: "cancel", target_request_id: turn2Id });
  const { events: afterFirstDelta, terminal: terminal2 } = await drive(turn2Id);
  // The first delta was consumed to trigger the cancel; keep it in the record.
  const events2: WorkerEvent[] = [firstWorkerEvent, ...afterFirstDelta];
  const cancelReply = await sink.next((f) => f.type === "reply" && f.request_id === cancelId);
  const turn2Requests = requests.slice(turn2Start);

  const finished2 = events2.find((e) => e.payload.type === "turn_finished");
  const error2 = events2.find((e) => e.payload.type === "error");
  const text2 = textOf(events2);

  evidence.abort = {
    request_id: turn2Id,
    prompt: prompt2,
    partial_text_before_abort: partialBeforeAbort,
    streamed_before_abort_chars: text2.length,
    cancel_reply: cancelReply.result,
    terminal_status: terminal2.result ?? terminal2.code,
    turn_finished_status: finished2?.payload.type === "turn_finished" ? finished2.payload.status : null,
    error_code: error2?.payload.type === "error" ? error2.payload.code : null,
    ends_as_cancelled_not_success: true,
    event_counts: summarize(events2),
    requests: turn2Requests,
  };

  expect("abort_cancel_reply_matched", resultOf(cancelReply).cancelled === true, JSON.stringify(cancelReply.result));
  expect("abort_was_mid_stream", partialBeforeAbort.length > 0, JSON.stringify(partialBeforeAbort));
  expect("abort_terminal_status_cancelled", resultOf(terminal2).status === "cancelled",
    `${terminal2.type} ${JSON.stringify(terminal2.result ?? terminal2.code)}`);
  expect("abort_finished_cancelled", finished2?.payload.type === "turn_finished" && finished2.payload.status === "cancelled",
    JSON.stringify(finished2?.payload));
  expect("abort_error_code_cancelled", error2?.payload.type === "error" && error2.payload.code === "cancelled",
    JSON.stringify(error2?.payload));
  expect("abort_never_succeeded", !events2.some((e) =>
    (e.payload.type === "turn_finished" && e.payload.status === "succeeded") || e.payload.type === "provider_message"));
  expect("abort_exactly_one_request", turn2Requests.length === 1, JSON.stringify(turn2Requests));

  // ── 4. Negative control: closed loopback port fails typed, no reroute ─────
  const closedConnection: ConnectionConfig = {
    id: "closed-loopback",
    protocol: "llama_cpp",
    endpoint: CLOSED_ENDPOINT,
    models: [{
      id: "closed-model",
      context_limit: 4_096,
      output_limit: 64,
      capabilities: [{
        name: "text",
        state: "qualified",
        implementation_version: CUSTOM_IMPLEMENTATION_VERSION,
        evidence_digest: probeDigest,
        observed_at: String(Date.now()),
      }],
    }],
    allowed_projects: [PROJECT],
  };
  const turn3Id = crypto.randomUUID();
  const input3 = turn(closedConnection, turn3Id, "hello", 16);
  const turn3Start = requests.length;
  send({ protocol: 1, request_id: turn3Id, method: "stream", input: input3 });
  const { events: events3, terminal: terminal3 } = await drive(turn3Id);
  const turn3Requests = requests.slice(turn3Start);

  const finished3 = events3.find((e) => e.payload.type === "turn_finished");
  const error3 = events3.find((e) => e.payload.type === "error");
  const typedCode = error3?.payload.type === "error" ? error3.payload.code : null;

  evidence.negative_control = {
    request_id: turn3Id,
    endpoint: CLOSED_ENDPOINT,
    terminal_status: terminal3.result ?? terminal3.code,
    turn_finished_status: finished3?.payload.type === "turn_finished" ? finished3.payload.status : null,
    error_code: typedCode,
    failed_typed_not_rerouted: true,
    event_counts: summarize(events3),
    requests: turn3Requests,
  };

  expect("control_did_not_succeed",
    resultOf(terminal3).status === "failed"
      && !events3.some((e) => e.payload.type === "turn_finished" && e.payload.status === "succeeded"),
    `${terminal3.type} ${JSON.stringify(terminal3.result ?? terminal3.code)}`);
  expect("control_finished_failed", finished3?.payload.type === "turn_finished" && finished3.payload.status === "failed",
    JSON.stringify(finished3?.payload));
  expect("control_typed_error", typedCode !== null && typedCode !== "cancelled", JSON.stringify(error3?.payload));
  expect("control_no_text_emitted", textOf(events3).length === 0);
  expect("control_attempted_closed_port_only",
    turn3Requests.length === 1 && turn3Requests[0]!.url.startsWith(`${CLOSED_ENDPOINT}/`), JSON.stringify(turn3Requests));
  expect("control_did_not_contact_the_good_endpoint",
    turn3Requests.every((request) => !request.url.startsWith(`${endpoint}/`)), JSON.stringify(turn3Requests));

  evidence.all_requests = requests;
  state = "passed";
} catch (error) {
  state = "failed";
  evidence.error = message(error);
} finally {
  try { inputController.close(); } catch { /* already closed */ }
  globalThis.fetch = realFetch;
  try { await worker; } catch (error) { evidence.worker_exit = message(error); }

  const artifacts: { path: string; sha256: string }[] = [];
  for (const relative of ["packages/contracts/src/generated.ts", "packages/contracts/schema.json"]) {
    try { artifacts.push({ path: relative, sha256: sha256(await readFile(resolve(ROOT, relative))) }); } catch { /* absent is absent */ }
  }
  const receipt = {
    gate: "qa/smoke/provider-roundtrip.ts",
    state,
    command: COMMAND,
    recorded_at: new Date().toISOString(),
    exit_code: state === "passed" ? 0 : 1,
    endpoint,
    mechanism: evidence.mechanism,
    assertions: checks,
    evidence,
    artifacts,
  };
  await mkdir(dirname(receiptPath), { recursive: true });
  await writeFile(receiptPath, `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(`provider roundtrip ${state}: ${checks.filter((c) => c.ok).length}/${checks.length} assertions passed`);
  console.log(`receipt written to ${receiptPath}`);
}

if (state !== "passed") process.exit(1);
