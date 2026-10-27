import { afterEach, expect, spyOn, test } from "bun:test";
import type { ConnectionConfig, ProviderProtocol, WorkerProviderTurnInput, WorkerEvent } from "../../contracts/src/generated.ts";
import { OmpProviderAdapter } from "../src/providers/omp-sdk.ts";
import { decodeFrames, encodeFrame, serveProviderRpc } from "../src/rpc.ts";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Writable } from "node:stream";

const servers: Bun.Server<unknown>[] = [];
afterEach(() => { for (const server of servers.splice(0)) server.stop(true); });
const sentinel = "credential_SENTINEL_do_not_emit";
function loopback(status = 200, body?: string, withholdHeadersUntilAbort = false) {
  const requests: { url: string; body: unknown; authorization: string | null }[] = [];
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
    requests.push({ url: request.url, body: await request.json(), authorization: request.headers.get("authorization") });
    if (withholdHeadersUntilAbort) await new Promise<void>(resolve => request.signal.addEventListener("abort", () => resolve(), { once: true }));
    return new Response(body ?? [
      'data: {"id":"fixture","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant","content":"actual stream"},"finish_reason":null}]}\n\n',
      'data: {"id":"fixture","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}\n\n',
      'data: [DONE]\n\n',
    ].join(""), { status, headers: { "content-type": "text/event-stream" } });
  } });
  servers.push(server);
  return { endpoint: `http://127.0.0.1:${server.port}/v1`, requests };
}
function config(endpoint: string, capabilities = ["text", "usage"]): ConnectionConfig {
  return { id: "selected", protocol: "openai_completions", endpoint, allowed_projects: ["project"],
    models: [{ id: "fixture-model", context_limit: 4096, output_limit: 128,
      capabilities: capabilities.map(name => ({ name, state: "qualified", implementation_version: "18.8.9", evidence_digest: "fixture-evidence", observed_at: "1791763200000" })) }] };
}
function input(): WorkerProviderTurnInput {
  return { run_id: "run", request_id: "request", message_id: "answer", route: { connection_id: "selected", model_id: "fixture-model" },
    messages: [{ role: "user", content: [{ type: "text", text: "fixture input" }] }], required_capabilities: ["text"], tool_schemas: [],
    disclosure_digest: "digest", credential_lease_id: "private-lease", connection: config("http://127.0.0.1:1/v1"),
    policy: { version: "1", project_id: "project", local_only: false, allowed_connection_ids: ["selected"], allowed_tools: ["read"],
      read_roots: [], write_roots: [], network_destinations: ["127.0.0.1"], max_workers: 1, max_requests: 1,
      spend_policy: "free_only", allow_unknown_cost: true } };
}
async function collect(adapter: OmpProviderAdapter, turn = input(), signal = new AbortController().signal) {
  const events: WorkerEvent[] = [];
  for await (const event of adapter.stream(turn, signal)) events.push(event);
  return events;
}
function adapter(connection: ConnectionConfig) {
  return new OmpProviderAdapter([connection], async () => new TextEncoder().encode(sentinel));
}
// Removing the qualification gate would issue HTTP and fail this test.
test("unsupported_tools_reject_before_request", async () => {
  const server = loopback(); const turn = input();
  turn.tool_schemas = [{ name: "read", description: "Read", input_schema: { type: "object", properties: {} } }];
  const events = await collect(adapter(config(server.endpoint)), turn);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error")).toBe(true);
});
// A fallback, or SDK HTTP replay, sends an extra request and fails.
test("outage_never_chooses_another_account", async () => {
  const first = loopback(503, "unavailable"); const other = loopback();
  const worker = new OmpProviderAdapter([config(first.endpoint), { ...config(other.endpoint), id: "other" }], async () => new TextEncoder().encode(sentinel));
  const turn = input(); const selectedRoute = structuredClone(turn.route);
  const events = await collect(worker, turn);
  expect(first.requests.length).toBe(1); expect(other.requests.length).toBe(0);
  expect(turn.route).toEqual(selectedRoute);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "failed")).toBe(true);
});
test("local_only_rejects_cloud_endpoint", async () => {
  const turn = input(); turn.policy.local_only = true;
  // Network-side throw double prevents an accidental cloud request. There is no
  // fake response; the real gate must make this transport entirely unreachable.
  const transport = spyOn(globalThis, "fetch").mockImplementation((async () => { throw new Error("unexpected outbound request"); }) as unknown as typeof globalThis.fetch);
  try {
    const events = await collect(adapter(config("https://example.invalid/v1")), turn);
    expect(transport.mock.calls.length).toBe(0);
    expect(events.some(e => e.payload.type === "error" && e.payload.code === "local_only_denied")).toBe(true);
  } finally { transport.mockRestore(); }
});
test("credential_sentinel_is_absent_from_events", async () => {
  const server = loopback(); const events = await collect(adapter(config(server.endpoint)));
  expect(server.requests[0]?.authorization).toBe(`Bearer ${sentinel}`);
  expect(events.some(e => e.payload.type === "text_delta" && e.payload.delta === "actual stream")).toBe(true);
  expect(JSON.stringify(events)).not.toContain(sentinel);
  expect(JSON.stringify(events)).not.toContain("private-lease");
});
test("provider_echoed_credential_fails_closed_without_emission", async () => {
  const body = `data: ${JSON.stringify({ id: "fixture", choices: [{ index: 0, delta: { content: sentinel }, finish_reason: "stop" }] })}\n\ndata: [DONE]\n\n`;
  const server = loopback(200, body); const events = await collect(adapter(config(server.endpoint)));
  expect(JSON.stringify(events)).not.toContain(sentinel);
  expect(events.some(e => e.payload.type === "error")).toBe(true);
});
test("usage_without_billing_is_unknown", async () => {
  const server = loopback(); const events = await collect(adapter(config(server.endpoint)));
  const usage = events.find(e => e.payload.type === "usage_observed");
  expect(usage?.payload).toMatchObject({ usage: { input_tokens: 7, output_tokens: 2, billing: { state: "unknown" } } });
});
test("tool_result_and_native_tool_call_cross_real_sdk_boundary", async () => {
  const body = 'data: {"id":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-2","type":"function","function":{"name":"read","arguments":"{\\"path\\":\\"file.txt\\"}"}}]},"finish_reason":null}]}\n\ndata: {"id":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}\n\ndata: [DONE]\n\n';
  const server = loopback(200, body); const turn = input();
  turn.tool_schemas = [{ name: "read", description: "Read", input_schema: { type: "object", properties: { path: { type: "string" } } } }];
  turn.messages = [{ role: "assistant", content: [], tool_calls: [{ id: "call-1", name: "read", arguments: { path: "earlier.txt" } }] },
    { role: "tool", tool_call_id: "call-1", content: [{ type: "text", text: "real tool output" }] }, ...turn.messages];
  const events = await collect(adapter(config(server.endpoint, ["text", "tools"])), turn);
  expect(server.requests[0]?.body).toMatchObject({ messages: expect.arrayContaining([{ role: "tool", tool_call_id: "call-1", content: "real tool output" }]) });
  expect(events.some(e => e.payload.type === "provider_tool_call" && e.payload.call.name === "read")).toBe(true);
  expect(events.some(e => e.payload.type === "tool_started")).toBe(false);
});
test("aborted_turn_does_not_issue_request", async () => {
  const server = loopback(); const abort = new AbortController(); abort.abort();
  const events = await collect(adapter(config(server.endpoint)), input(), abort.signal);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "cancelled")).toBe(true);
});
test("rpc_rejects_oversize_and_partial_frames", async () => {
  async function* bytes(value: string) { yield new TextEncoder().encode(value); }
  const drain = async (value: string) => { for await (const _ of decodeFrames(bytes(value), 64)) {} };
  await expect(drain("x".repeat(65))).rejects.toThrow();
  await expect(drain('{"protocol":1}')).rejects.toThrow();
  expect(() => encodeFrame({ value: "x".repeat(65) }, 64)).toThrow();
});

// Fixtures live at the HTTP boundary. They exercise the package's actual SDK
// serializers/parsers, not a mocked ProviderAdapter or a copy of its source.
const protocolFixtures: { protocol: ProviderProtocol; suffix: string; body: string }[] = [
  { protocol: "gemini", suffix: "/v1/models/fixture-model:streamGenerateContent", body:
    'data: {"candidates":[{"content":{"role":"model","parts":[{"text":"actual stream"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2,"totalTokenCount":9}}\n\n' },
  { protocol: "ollama", suffix: "/api/chat", body:
    '{"message":{"role":"assistant","content":"actual stream"},"done":false}\n{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":7,"eval_count":2}\n' },
  { protocol: "anthropic", suffix: "/v1/messages", body: [
    'event: message_start\ndata: {"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","model":"fixture-model","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":0}}}\n\n',
    'event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n',
    'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"actual stream"}}\n\n',
    'event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n',
    'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}\n\n',
    'event: message_stop\ndata: {"type":"message_stop"}\n\n',
  ].join("") },
  { protocol: "openai_responses", suffix: "/v1/responses", body: [
    'data: {"type":"response.created","response":{"id":"fixture","status":"in_progress","output":[]}}\n\n',
    'data: {"type":"response.output_item.added","output_index":0,"item":{"id":"msg","type":"message","role":"assistant","status":"in_progress","content":[]}}\n\n',
    'data: {"type":"response.content_part.added","item_id":"msg","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}\n\n',
    'data: {"type":"response.output_text.delta","item_id":"msg","output_index":0,"content_index":0,"delta":"actual stream"}\n\n',
    'data: {"type":"response.output_text.done","item_id":"msg","output_index":0,"content_index":0,"text":"actual stream"}\n\n',
    'data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"actual stream","annotations":[]}]}}\n\n',
    'data: {"type":"response.completed","response":{"id":"fixture","status":"completed","output":[{"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"actual stream","annotations":[]}]}],"usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}}\n\n',
  ].join("") },
];
for (const fixture of protocolFixtures) {
  test(`${fixture.protocol}_actual_sdk_request_and_stream_boundary`, async () => {
    const server = loopback(200, fixture.body);
    const connection = { ...config(fixture.protocol === "ollama" ? server.endpoint.replace(/\/v1$/, "") : server.endpoint), protocol: fixture.protocol };
    const turn = input(); turn.connection = connection;
    const events = await collect(adapter(connection), turn);
    expect(server.requests.length).toBe(1);
    expect(new URL(server.requests[0]!.url).pathname).toBe(fixture.suffix);
    expect(events.filter(e => e.payload.type === "text_delta").map(e => e.payload.type === "text_delta" ? e.payload.delta : "").join("")).toBe("actual stream");
    expect(events.find(e => e.payload.type === "usage_observed")?.payload).toMatchObject({ usage: { input_tokens: 7, output_tokens: 2, billing: { state: "unknown" } } });
    expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "succeeded")).toBe(true);
  });
}
test("unactivated_custom_connection_denies_before_request", async () => {
  const server = loopback();
  const connection = { ...config(server.endpoint), protocol: "custom" as const };
  const turn = input(); turn.connection = connection;
  const events = await collect(adapter(connection), turn);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error")).toBe(true);
});
test("redirect_cannot_disclose_selected_credential_to_other_origin", async () => {
  const other = loopback();
  const redirect = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response(null, { status: 307, headers: { location: other.endpoint + "/chat/completions" } }) });
  servers.push(redirect);
  const events = await collect(adapter(config(`http://127.0.0.1:${redirect.port}/v1`)));
  expect(other.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error")).toBe(true);
});
test("keyless_local_stream_does_not_invent_or_inherit_credentials", async () => {
  const server = loopback(); const connection = config(server.endpoint);
  const events = await collect(new OmpProviderAdapter([connection], async () => new Uint8Array(0)));
  expect(server.requests[0]?.authorization).toBeNull();
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "succeeded")).toBe(true);
});
test("credentials_are_zeroed_after_provider_completion", async () => {
  const server = loopback(); const bytes = new TextEncoder().encode(sentinel);
  await collect(new OmpProviderAdapter([config(server.endpoint)], async () => bytes));
  expect(Array.from(bytes).every(value => value === 0)).toBe(true);
});
test("tool_use_history_requires_qualification_even_without_new_tools", async () => {
  const server = loopback(); const turn = input();
  turn.messages.unshift({ role: "assistant", content: [], tool_calls: [{ id: "call", name: "read", arguments: {} }] });
  const events = await collect(adapter(config(server.endpoint)), turn);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error" && e.payload.code === "unqualified_capability")).toBe(true);
});
test("cancel_interrupts_an_active_actual_http_stream", async () => {
  let requested!: () => void;
  const started = new Promise<void>(resolve => { requested = resolve; });
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
    await request.json(); requested();
    return new Response(new ReadableStream<Uint8Array>({ start(controller) {
      controller.enqueue(new TextEncoder().encode('data: {"id":"fixture","choices":[{"index":0,"delta":{"role":"assistant","content":"partial"},"finish_reason":null}]}\n\n'));
    } }), { headers: { "content-type": "text/event-stream" } });
  } });
  servers.push(server);
  const abort = new AbortController();
  const pending = collect(adapter(config(`http://127.0.0.1:${server.port}/v1`)), input(), abort.signal);
  await started; abort.abort();
  const events = await pending;
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "cancelled")).toBe(true);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "succeeded")).toBe(false);
});

// Real wall-clock behavior is exercised deliberately: the SDK first-event
// watchdog must interrupt an actual HTTP request whose headers never arrive.
test("first_event_watchdog_aborts_a_stalled_openai_request_without_retry", async () => {
  const server = loopback(200, undefined, true);
  const events = await collect(adapter(config(server.endpoint)), input());
  expect(server.requests.length).toBe(1);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "failed")).toBe(true);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "cancelled")).toBe(false);
}, 30_000);
test("first_event_watchdog_aborts_a_stalled_ollama_request_without_retry", async () => {
  const requests: string[] = [];
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
    requests.push(request.url);
    return new Response(new ReadableStream<Uint8Array>(), { headers: { "content-type": "application/x-ndjson" } });
  } });
  servers.push(server);
  const connection = { ...config(`http://127.0.0.1:${server.port}/api`, ["text", "usage"]), protocol: "ollama" as const };
  const turn = input();
  turn.connection = connection;
  const events = await collect(adapter(connection), turn);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "failed")).toBe(true);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "cancelled")).toBe(false);
  expect(requests.length).toBe(1);
}, 30_000);
test("completed_oversized_upstream_line_fails_closed_before_json_allocation", async () => {
  const oversized = 'data: {"id":"fixture","choices":[{"index":0,"delta":{"content":"' + "x".repeat(1_100_000) + '"},"finish_reason":"stop"}]}\n\n';
  const server = loopback(200, oversized);
  const events = await collect(adapter(config(server.endpoint)));
  expect(server.requests.length).toBe(1);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "failed")).toBe(true);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "succeeded")).toBe(false);
});
test("oversized_upstream_line_split_across_chunks_fails_closed_before_json_allocation", async () => {
  const oversized = 'data: {"id":"fixture","choices":[{"index":0,"delta":{"content":"' + "x".repeat(1_100_000) + '"},"finish_reason":"stop"}]}\n\n';
  const requests: string[] = [];
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
    requests.push(request.url);
    return new Response(
      new ReadableStream<Uint8Array>({ start(controller) {
        const bytes = new TextEncoder().encode(oversized);
        for (let offset = 0; offset < bytes.length; offset += 10_000) controller.enqueue(bytes.subarray(offset, offset + 10_000));
        controller.close();
      } }), { headers: { "content-type": "text/event-stream" } });
  } });
  servers.push(server);
  const events = await collect(adapter(config(`http://127.0.0.1:${server.port}/v1`)));
  expect(requests.length).toBe(1);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "failed")).toBe(true);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "succeeded")).toBe(false);
});
test("cumulative_upstream_output_bound_fails_closed_without_second_request", async () => {
  const chunks = Array.from({ length: 4097 }, () => 'data: {"id":"fixture","choices":[{"index":0,"delta":{"content":"' + "x".repeat(2000) + '"},"finish_reason":null}]}\n\n').join("");
  const server = loopback(200, chunks);
  const events = await collect(adapter(config(server.endpoint)));
  expect(server.requests.length).toBe(1);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "failed")).toBe(true);
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "succeeded")).toBe(false);
}, 30_000);

// Backpressure proof: the upstream streams 300x100 KB chunks paced 1.2 s
// apart. The RPC writer is blocked after 6 frames, so the SDK idle timeout
// aborts the turn while the terminal frame cannot be written. An unbounded
// in-memory event backlog would keep buffering (and, after release, deliver a
// terminal frame); a bounded worker must never settle while its writer is
// blocked and must never emit a succeeded terminal.
test("blocked_downstream_writer_binds_the_producer_no_unbounded_backlog", async () => {
  const turn = input();
  const chunk = 'data: {"id":"fixture","choices":[{"index":0,"delta":{"content":"' + "x".repeat(100_000) + '"},"finish_reason":null}]}\n\n';
  const requests: string[] = [];
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
    requests.push(request.url);
    return new Response(
      new ReadableStream<Uint8Array>({ async start(controller) {
        for (let index = 0; index < 300; index++) {
          controller.enqueue(new TextEncoder().encode(chunk));
          const tick = Promise.withResolvers<void>();
          setTimeout(tick.resolve, 1200);
          await tick.promise;
        }
        controller.close();
      } }), { headers: { "content-type": "text/event-stream" } });
  } });
  servers.push(server);
  const connection = config(`http://127.0.0.1:${server.port}/v1`);
  const writes: Uint8Array[] = [];
  const gate = Promise.withResolvers<void>();
  const writer = async (bytes: Uint8Array) => {
    writes.push(bytes);
    if (writes.length === 6) await gate.promise;
  };
  const frames: Uint8Array[] = [
    new TextEncoder().encode('{"protocol":1,"request_id":"request","method":"hello"}\n'),
    new TextEncoder().encode(JSON.stringify({ protocol: 1, request_id: turn.request_id, method: "stream", input: { ...turn, connection } }) + "\n"),
  ];
  // stdin stays open while a turn runs; model that or the RPC service tears
  // down its active task the moment the source iterator ends.
  const task = serveProviderRpc((async function* () {
    for (const frame of frames) yield frame;
    await gate.promise;
  })(), writer, () => adapter(connection));
  let settled = false;
  const done = task.then(() => { settled = true; }, () => { settled = true; });
  const deadline = Date.now() + 20_000;
  while (writes.length < 6 && Date.now() < deadline) {
    const tick = Promise.withResolvers<void>();
    setTimeout(tick.resolve, 1);
    await tick.promise;
  }
  expect(writes.length).toBe(6);
  await Bun.sleep(10_000);
  expect(settled).toBe(false);
  gate.resolve();
  await done;
  expect(requests.length).toBe(1);
  const text = writes.map(frame => new TextDecoder().decode(frame)).join("");
  expect(text).not.toContain('"status":"succeeded"');
}, 90_000);
test("stdio_worker_consumes_private_binary_credential_lease_without_json_secrets", async () => {
  const server = loopback(); const directory = mkdtempSync(join(tmpdir(), "ironbark-provider-worker-"));
  const child = spawn(process.execPath, [new URL("../src/main.ts", import.meta.url).pathname], {
    env: { PATH: process.env.PATH, IRONBARK_WORKER_HOME: directory, IRONBARK_WORKER_TMPDIR: directory },
    stdio: ["pipe", "pipe", "pipe", "pipe"],
  });
  const exit = new Promise<number | null>((resolve, reject) => { child.on("error", reject); child.on("exit", resolve); });
  let stderr = ""; child.stderr!.on("data", chunk => { stderr += String(chunk); });
  try {
    const turn = input(); turn.connection = config(server.endpoint);
    const id = new TextEncoder().encode(turn.credential_lease_id), key = new TextEncoder().encode(sentinel);
    const lease = new Uint8Array(8 + id.length + key.length);
    const header = new DataView(lease.buffer); header.setUint32(0, id.length); header.setUint32(4, key.length);
    lease.set(id, 8); lease.set(key, 8 + id.length);
    (child.stdio[3] as Writable).end(lease);
    child.stdin!.write(encodeFrame({ protocol: 1, request_id: "hello", method: "hello" }));
    child.stdin!.write(encodeFrame({ protocol: 1, request_id: turn.request_id, method: "stream", input: turn }));
    const frames: unknown[] = [];
    for await (const frame of decodeFrames(child.stdout!)) {
      frames.push(frame);
      if (typeof frame === "object" && frame !== null && "request_id" in frame && frame.request_id === turn.request_id
        && "type" in frame && frame.type === "reply") child.stdin!.end();
    }
    expect(await exit).toBe(0);
    expect(JSON.stringify(frames)).not.toContain(sentinel);
    expect(JSON.stringify(frames)).not.toContain("private-lease");
    expect(stderr).not.toContain(sentinel);
    expect(server.requests[0]?.authorization).toBe(`Bearer ${sentinel}`);
    expect(frames).toEqual(expect.arrayContaining([expect.objectContaining({ type: "reply", request_id: "request", result: { status: "succeeded" } })]));
    lease.fill(0); key.fill(0);
  } finally {
    child.kill("SIGKILL"); child.stdin?.destroy();
    rmSync(directory, { recursive: true, force: true });
  }
}, 15000);
test("usage_missing_on_wire_remains_unknown_not_sdk_zero_estimate", async () => {
  const server = loopback(200, 'data: {"id":"fixture","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n');
  const events = await collect(adapter(config(server.endpoint)));
  expect(events.find(e => e.payload.type === "usage_observed")?.payload).toEqual({ type: "usage_observed", usage: { source: "unknown", billing: { state: "unknown" } } });
});
test("native_signed_replay_cannot_cross_connection_or_model", async () => {
  const server = loopback(); const turn = input();
  turn.messages.unshift({ role: "assistant", content: [{ type: "text", text: "prior" }],
    sdk_content: { blocks: [{ type: "thinking", thinking: "reason", thinkingSignature: "signature" }] },
    sdk_route: { connection_id: "other", model_id: "fixture-model" } });
  const events = await collect(adapter(config(server.endpoint)), turn);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error" && e.payload.code === "native_replay_route_mismatch")).toBe(true);
});

// P1-3: a same-route native replay must be an exact one-to-one projection of the
// canonical calls, including structurally-equal JSON arguments. Route equality
// alone is not transcript equality.
function replayTurn(server: { endpoint: string }, nativeArguments: unknown, omitBlock = false, duplicate = false) {
  const turn = input();
  turn.messages = [{ role: "assistant", content: [],
    tool_calls: [{ id: "call-1", name: "read", arguments: { path: "safe.txt" } }],
    sdk_content: { blocks: omitBlock ? [] : [{ type: "toolCall", id: "call-1", name: "read", arguments: nativeArguments }, ...(duplicate ? [{ type: "toolCall", id: "call-1", name: "read", arguments: { path: "safe.txt" } }] : [])] },
    sdk_route: { connection_id: "selected", model_id: "fixture-model" } },
    { role: "tool", tool_call_id: "call-1", content: [{ type: "text", text: "real tool output" }] },
    { role: "user", content: [{ type: "text", text: "fixture input" }] }];
  return turn;
}
test("same_route_native_replay_with_mismatched_arguments_fails_before_request", async () => {
  const server = loopback(); const turn = replayTurn(server, { path: "different.txt" });
  const events = await collect(adapter(config(server.endpoint, ["text", "tools"])), turn);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error" && e.payload.code === "native_tool_replay_mismatch")).toBe(true);
});
test("same_route_native_replay_omitting_a_canonical_call_fails_before_request", async () => {
  const server = loopback(); const turn = replayTurn(server, { path: "safe.txt" }, true);
  const events = await collect(adapter(config(server.endpoint, ["text", "tools"])), turn);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error" && e.payload.code === "native_tool_replay_mismatch")).toBe(true);
});
test("same_route_native_replay_duplicating_a_canonical_call_fails_before_request", async () => {
  const server = loopback(); const turn = replayTurn(server, { path: "safe.txt" }, false, true);
  const events = await collect(adapter(config(server.endpoint, ["text", "tools"])), turn);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error" && e.payload.code === "native_tool_replay_mismatch")).toBe(true);
});
test("exact_native_tool_call_replay_crosses_real_sdk_boundary_unchanged", async () => {
  const server = loopback(); const turn = replayTurn(server, { path: "safe.txt" });
  const events = await collect(adapter(config(server.endpoint, ["text", "tools"])), turn);
  expect(server.requests.length).toBe(1);
  expect(JSON.stringify(server.requests[0]?.body)).toContain("safe.txt");
  expect(events.some(e => e.payload.type === "turn_finished" && e.payload.status === "succeeded")).toBe(true);
});
test("split_and_json_escaped_credential_echo_is_not_visible", async () => {
  const key = 'credential_"escaped"_sentinel';
  const first = key.slice(0, 12), second = key.slice(12);
  const body = [first, second].map((content, index) => `data: ${JSON.stringify({ id: "fixture", choices: [{ index: 0, delta: { content }, finish_reason: index ? "stop" : null }] })}\n\n`).join("") + "data: [DONE]\n\n";
  const server = loopback(200, body);
  const events = await collect(new OmpProviderAdapter([config(server.endpoint)], async () => new TextEncoder().encode(key)));
  const visible = events.map(event => event.payload.type === "text_delta" ? event.payload.delta : "").join("");
  expect(visible).not.toContain(key);
  expect(events.some(e => e.payload.type === "error" && e.payload.code === "credential_disclosure_denied")).toBe(true);
});
test("private_connection_protocol_cannot_override_registered_route", async () => {
  const server = loopback(); const turn = input();
  turn.connection.protocol = "custom";
  const events = await collect(adapter(config(server.endpoint)), turn);
  expect(server.requests.length).toBe(0);
  expect(events.some(e => e.payload.type === "error" && e.payload.code === "route_denied")).toBe(true);
});
