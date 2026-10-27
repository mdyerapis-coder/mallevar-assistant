import { strict as assert } from "node:assert";
import { resolve } from "node:path";
import type { CoreCommand, ConversationHistory, QueuedTurn, BranchId, MessageView, EventEnvelope } from "../../packages/contracts/src/generated";

// Parent exports these DTOs from Rust and implements generic CLI `command`.
// This smoke never installs/builds binaries, fabricates worker events, or uses a
// canned provider. Run only against an explicitly owner-approved connection.
const args = process.argv.slice(2);
function option(name: string): string | undefined { const i = args.indexOf(name); return i < 0 ? undefined : args[i + 1]; }
const connection = option("--connection");
const model = option("--model");
const project = option("--project");
const binary = resolve(process.env.IRONBARK_CLI_BIN ?? "target/debug/ironbark-cli");
const report = ".superpowers/sdd/2026-10-12-ironbark-assistant/P2-smoke.json";
const evidence: Record<string, unknown> = { state: "running", mechanism: "actual prebuilt CLI and registered provider", connection, model, project, simulated_model_success: false };

async function cli<T>(command: CoreCommand, requestId = crypto.randomUUID()): Promise<T> {
  const child = Bun.spawn([binary, "command", "--request-id", requestId, JSON.stringify(command)], { stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGTERM"), 120_000);
  try {
    const [out, err, status] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
    if (status !== 0) throw new Error(`CLI failure (${status}): ${err.slice(0, 512)}`);
    // The CLI prints the typed leaf value, not the CoreReply envelope
    // (crates/ironbark-cli emit(): leaf_json); a core failure exits non-zero.
    return JSON.parse(out) as T;
  } finally { clearTimeout(timer); }
}
const parts = (text: string) => [{ type: "text" as const, text }];
const projectedText = (message: MessageView) => message.parts.filter((p) => p.type === "text").map((p) => p.type === "text" ? p.text : "").join("");
function receipts(history: ConversationHistory, run: string): EventEnvelope[] {
  const bySequence = new Map<string, EventEnvelope>();
  for (const branch of history.branches) for (const message of branch.messages) for (const event of message.receipts) if (event.run_id === run) bySequence.set(event.seq, event);
  return [...bySequence.values()].sort((a, b) => (BigInt(a.seq) > BigInt(b.seq) ? 1 : BigInt(a.seq) < BigInt(b.seq) ? -1 : 0));
}
async function poll(conversation: string, predicate: (history: ConversationHistory) => boolean, timeout = 120_000): Promise<ConversationHistory> {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const history = await cli<ConversationHistory>({ type: "conversation_history", conversation_id: conversation });
    if (predicate(history)) return history;
    await Bun.sleep(100);
  }
  throw new Error("actual conversation did not reach required observed state before deadline");
}
function response(history: ConversationHistory, turn: QueuedTurn): MessageView {
  const value = history.branches.find((b) => b.id === turn.branch_id)?.messages.find((m) => m.id === turn.assistant_message_id);
  assert(value, "durable assistant projection must exist");
  return value;
}
let active: QueuedTurn | undefined;
try {
  assert(connection && model && project, "--connection, --model and --project are required; use an owner-approved configured local route/project");
  const pin = { connection_id: connection, model_id: model };
  const conversation = await cli<string>({ type: "conversation_create", project_id: project });
  const initial = await cli<ConversationHistory>({ type: "conversation_history", conversation_id: conversation });
  const branch = initial.branches[0]!.id;
  const request = crypto.randomUUID();
  const queueCommand: CoreCommand = { type: "conversation_queue_turn", conversation_id: conversation, branch_id: branch, parts: parts("Write a detailed numbered list of 300 different workshop safety practices, one line each. This is a transport interruption test; no tools or private sources."), pin };
  const original = await cli<QueuedTurn>(queueCommand, request);
  active = original;
  const streamed = await poll(conversation, (history) => {
    const events = receipts(history, original.run_id);
    return events.some((e) => e.payload.type === "text_delta" && e.payload.delta.length > 0) && response(history, original).status === "streaming";
  });
  assert(projectedText(response(streamed, original)).length > 0, "must interrupt a genuine partial response, not a fabricated empty stream");
  // This command must await native process/transport observed-stop and C4
  // reconciliation. A cancel-request-only implementation fails below.
  const stopReceipt = await cli<unknown>({ type: "run_interrupt", run_id: original.run_id });
  active = undefined;
  const stopped = await poll(conversation, (history) => response(history, original).status === "interrupted");
  const oldBranch = stopped.branches.find((b) => b.id === branch)!;
  const partial = projectedText(response(stopped, original));
  assert(partial.length > 0);
  const beforeReplay = receipts(stopped, original.run_id);
  assert(!beforeReplay.some((e) => e.payload.type === "turn_finished" && e.payload.status === "succeeded"), "interrupted native stream must not become successful");
  // Each CLI process is a new socket connection: this exercises reconnect and
  // durable request identity at the real command sink.
  const replay = await cli<QueuedTurn>(queueCommand, request);
  assert.equal(replay.run_id, original.run_id);
  const reconnected = await cli<ConversationHistory>({ type: "conversation_history", conversation_id: conversation });
  assert.deepEqual(receipts(reconnected, original.run_id), beforeReplay);
  assert.equal(reconnected.branches.find((b) => b.id === branch)!.messages.length, 2);

  const edited = await cli<BranchId>({ type: "conversation_edit", message_id: original.user_message_id, parts: parts("Explain three workshop safety practices briefly. Do not use tools.") });
  assert.notEqual(edited, branch);
  const editedTurn = await cli<QueuedTurn>({ type: "conversation_queue_turn", conversation_id: conversation, branch_id: edited, parts: parts("Explain three workshop safety practices briefly. Do not use tools."), pin });
  active = editedTurn;
  const editedHistory = await poll(conversation, (history) => response(history, editedTurn).status === "complete");
  active = undefined;
  assert(projectedText(response(editedHistory, editedTurn)).length > 0, "edited branch requires actual accepted provider output");
  assert.deepEqual(editedHistory.branches.find((b) => b.id === branch), oldBranch);
  assert.deepEqual(receipts(editedHistory, original.run_id), beforeReplay);
  const regenerated = await cli<QueuedTurn>({ type: "conversation_regenerate", message_id: editedTurn.assistant_message_id, pin });
  active = regenerated;
  assert.notEqual(regenerated.branch_id, edited);
  const finalHistory = await poll(conversation, (history) => response(history, regenerated).status === "complete");
  active = undefined;
  assert(projectedText(response(finalHistory, regenerated)).length > 0, "regeneration must run the actual provider on its own branch");
  assert.equal(regenerated.user_message_id, editedTurn.user_message_id);
  assert.notEqual(regenerated.assistant_message_id, editedTurn.assistant_message_id);
  assert.deepEqual(finalHistory.branches.find((b) => b.id === branch), oldBranch);
  assert.deepEqual(receipts(finalHistory, original.run_id), beforeReplay);
  evidence.state = "passed";
  Object.assign(evidence, { conversation, original, edited: editedTurn, regenerated, partial_characters: partial.length, original_event_sequences: beforeReplay.map((e) => e.seq), reconnect: "same run/no new events", observed_stop_receipt: stopReceipt, historical_tool_receipts: "preserved if actually emitted; chat-only smoke does not fabricate tools", original_branch: "immutable", activation: "actual accepted answers on edited/regenerated branches", cleanup: "conversation retained for owner evidence; no owner data removed" });
} catch (error) {
  evidence.state = "blocked_or_failed";
  const message = error instanceof Error ? error.message : "unknown smoke error";
  evidence.error = message.replace(/(bearer\s+|(?:api[_-]?key|password|token|credential_lease_id)\s*[:=]\s*)[^\s,;]+/gi, "$1[REDACTED]");
  process.exitCode = 1;
} finally {
  if (active) {
    try { evidence.failure_stop_receipt = await cli<unknown>({ type: "run_interrupt", run_id: active.run_id }); }
    catch { evidence.failure_stop = "unreconciled; owner must inspect run"; }
  }
  await Bun.write(report, JSON.stringify(evidence, null, 2) + "\n");
  console.log(JSON.stringify(evidence));
}
