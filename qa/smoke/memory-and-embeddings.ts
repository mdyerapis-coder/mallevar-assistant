//! P5 memory + embeddings smoke: drives the real prebuilt core+CLI inside an
//! isolated XDG sandbox. Proves a proposed memory is NOT retrievable until the
//! owner approves it, that forgetting removes it from retrieval while the audit
//! record survives, and then exercises a real, versioned local embedding index
//! over an ingested revision.
import { strict as assert } from "node:assert";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { Subprocess } from "bun";
import type { CoreCommand, EmbeddingIndexReceipt, EmbeddingModelIdentity, MemoryRecord, Project, SourceRevision } from "../../packages/contracts/src/generated";

const projectFlag = process.argv.indexOf("--project");
const requestedProject = projectFlag < 0 ? undefined : process.argv[projectFlag + 1];
const binary = resolve(process.env.IRONBARK_CLI_BIN ?? "target/debug/ironbark-cli");
const coreBinary = resolve(process.env.IRONBARK_CORE_BIN ?? "target/debug/ironbark-core");
const reportDir = ".superpowers/sdd/2026-10-12-ironbark-assistant";
const report = `${reportDir}/P5-memory-embeddings.json`;
const evidence: Record<string, unknown> = {
  state: "running",
  mechanism: "actual prebuilt core + CLI in an isolated XDG sandbox; real memory + embed index",
  requested_project: requestedProject ?? null,
  simulated_model_success: false,
};

/// The isolated XDG environment every spawned process must share: the CLI must
/// talk to the sandboxed daemon, never to whatever daemon the developer's own
/// environment happens to have running.
let sandbox: NodeJS.ProcessEnv | undefined;

interface CliOutcome<T> { status: number; value?: T; code?: string; message?: string }

async function call<T>(command: CoreCommand): Promise<CliOutcome<T>> {
  assert(sandbox, "the sandbox environment must be established before any CLI call");
  const child = Bun.spawn([binary, "command", "--request-id", crypto.randomUUID(), JSON.stringify(command)], { env: sandbox, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGTERM"), 60_000);
  try {
    const [stdout, stderr, status] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
    if (status === 0) return { status, value: JSON.parse(stdout.trim()) as T };
    const match = /^([a-z0-9_]+): ([\s\S]*)$/.exec(stderr.trim());
    return { status, code: match?.[1] ?? "untyped_error", message: match?.[2] ?? (stderr.trim() || stdout.trim()) };
  } finally { clearTimeout(timer); }
}

async function cli<T>(command: CoreCommand): Promise<T> {
  const outcome = await call<T>(command);
  if (outcome.status !== 0) throw new Error(`${command.type} failed (${outcome.code}): ${outcome.message}`);
  return outcome.value as T;
}

async function readUntilReady(stream: ReadableStream<Uint8Array>, predicate: (line: string) => boolean): Promise<boolean> {
  const decoder = new TextDecoder();
  let buffer = "";
  const reader = stream.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value) buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() ?? "";
      for (const line of lines) if (predicate(line.trim())) return true;
    }
  } finally { reader.releaseLock(); }
  return predicate(buffer.trim());
}

async function startCore(): Promise<Subprocess<"ignore", "pipe", "pipe">> {
  assert(sandbox, "the sandbox environment must exist before starting the core");
  const child = Bun.spawn([coreBinary], { env: sandbox, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGTERM"), 10_000);
  try {
    if (await readUntilReady(child.stdout, (line) => line.includes("CORE_READY"))) return child;
    const stderr = await new Response(child.stderr).text();
    throw new Error(`core did not report ready: ${stderr.trim()}`);
  } finally { clearTimeout(timer); }
}

let core: Subprocess<"ignore", "pipe", "pipe"> | undefined;
let temp: string | undefined;

try {
  temp = await mkdtemp(join(tmpdir(), "ibp5-"));
  const env: NodeJS.ProcessEnv = { ...process.env, HOME: temp, XDG_DATA_HOME: join(temp, "d"), XDG_STATE_HOME: join(temp, "s"), XDG_RUNTIME_DIR: join(temp, "r") };
  for (const dir of [env.XDG_DATA_HOME!, env.XDG_STATE_HOME!, env.XDG_RUNTIME_DIR!]) await mkdir(dir, { recursive: true, mode: 0o700 });
  sandbox = env;

  core = await startCore();

  const root = join(temp, "root");
  await mkdir(root, { recursive: true });
  const projectId: string = requestedProject ?? (await cli<Project>({ type: "project_create", name: "memory-embeddings", root })).id;

  // ── explicit owner memory: candidate -> approved -> forgotten ──
  const candidateId = await cli<string>({ type: "memory_propose", project_id: projectId, text: "test memory fact", source: "smoke test" });
  assert(candidateId.length > 0, "the core must assign a candidate id");

  const before = await cli<MemoryRecord[]>({ type: "knowledge_search_memory", project_id: projectId, query: "test", limit: 10 });
  assert.equal(before.length, 0, "a proposed memory must NOT be retrievable before approval");

  const approved = await cli<MemoryRecord>({ type: "memory_decide", project_id: projectId, candidate_id: candidateId, decision: "approve" });
  assert.equal(approved.state, "approved", "an approve decision must publish the memory");
  assert.equal(approved.text, "test memory fact", "approval keeps the original text");

  const after = await cli<MemoryRecord[]>({ type: "knowledge_search_memory", project_id: projectId, query: "test", limit: 10 });
  assert.equal(after.length, 1, "an approved memory must be retrievable");
  assert.equal(after[0].text, "test memory fact");

  const forgotten = await cli<MemoryRecord>({ type: "memory_forget", project_id: projectId, record_id: approved.id });
  assert.equal(forgotten.state, "forgotten", "forgetting marks the record, it does not destroy it");

  const gone = await cli<MemoryRecord[]>({ type: "knowledge_search_memory", project_id: projectId, query: "test", limit: 10 });
  assert.equal(gone.length, 0, "a forgotten memory must not be retrievable");

  // ── a real, versioned local embedding index over an ingested revision ──
  const fixtureDir = join(temp, "fixtures");
  await mkdir(fixtureDir, { recursive: true });
  const fixturePath = join(fixtureDir, "index.md");
  await writeFile(fixturePath, new TextEncoder().encode("# Workshop\nembedding fixture body about workshop topics\n"));
  const revision = await cli<SourceRevision>({ type: "knowledge_ingest", project_id: projectId, source: { type: "file", path: fixturePath }, disclosure: { type: "explicit_public" } });

  const model: EmbeddingModelIdentity = { connection_id: "local", model_id: "hash-bucket", version: "1", dimensions: 8 };
  const receipt = await cli<EmbeddingIndexReceipt>({ type: "knowledge_embed_index", project_id: projectId, model });
  assert(receipt.chunks_indexed > 0, "indexing a revision with real chunks must write vectors");

  // The core's `knowledge_embed_query` reply DTO is not exported through
  // packages/contracts/src/generated.ts, so the raw reply is recorded rather
  // than asserted against a second, drifting contract.
  const query = await call<unknown>({ type: "knowledge_embed_query", project_id: projectId, model, query: "workshop" });
  const queryEvidence = query.status === 0
    ? { outcome: "ok", reply: query.value }
    : { outcome: "refused", code: query.code, message: query.message };

  evidence.state = "passed";
  Object.assign(evidence, {
    candidate_id: candidateId,
    record_id: approved.id,
    source_id: revision.source_id,
    revision_digest: revision.digest,
    indexed_chunks: receipt.chunks_indexed,
    indexed_at: receipt.indexed_at,
    embed_query: queryEvidence,
  });
} catch (error) {
  evidence.state = "failed";
  evidence.error = error instanceof Error ? error.message : String(error);
  process.exitCode = 1;
} finally {
  if (core) { core.kill("SIGTERM"); await core.exited.catch(() => {}); }
  if (temp) await rm(temp, { recursive: true, force: true });
  await mkdir(reportDir, { recursive: true });
  await writeFile(report, JSON.stringify(evidence, null, 2) + "\n");
  console.log(JSON.stringify(evidence));
}
