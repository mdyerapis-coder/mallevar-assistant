//! P4 retrieval + disclosure smoke: drives the real prebuilt core+CLI inside an
//! isolated XDG sandbox. Two projects share an identical title; a LocalOnly
//! source ingested into one must never resolve through the other (project-scoped
//! disclosure filtering), and deleting it resolves an explicit `Deleted` for its
//! own historic citation — never a silent current-revision substitution.
import { strict as assert } from "node:assert";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { Subprocess } from "bun";
import type { Citation, CitationView, CitedExcerpt, CoreCommand, DeletionReceipt, Project, SourceRevision } from "../../packages/contracts/src/generated";

const projectFlag = process.argv.indexOf("--project");
const requestedProject = projectFlag < 0 ? undefined : process.argv[projectFlag + 1];
const binary = resolve(process.env.IRONBARK_CLI_BIN ?? "target/debug/ironbark-cli");
const coreBinary = resolve(process.env.IRONBARK_CORE_BIN ?? "target/debug/ironbark-core");
const reportDir = ".superpowers/sdd/2026-10-12-ironbark-assistant";
const report = `${reportDir}/P4-retrieval-disclosure.json`;
const evidence: Record<string, unknown> = {
  state: "running",
  mechanism: "actual prebuilt core + CLI in an isolated XDG sandbox; real disclosure filtering",
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
  temp = await mkdtemp(join(tmpdir(), "ibp4-"));
  const env: NodeJS.ProcessEnv = { ...process.env, HOME: temp, XDG_DATA_HOME: join(temp, "d"), XDG_STATE_HOME: join(temp, "s"), XDG_RUNTIME_DIR: join(temp, "r") };
  for (const dir of [env.XDG_DATA_HOME!, env.XDG_STATE_HOME!, env.XDG_RUNTIME_DIR!]) await mkdir(dir, { recursive: true, mode: 0o700 });
  sandbox = env;

  core = await startCore();

  // Two projects with an IDENTICAL title: filtering must be by project identity,
  // never by the display name.
  const rootA = join(temp, "alpha");
  const rootB = join(temp, "beta");
  await mkdir(rootA, { recursive: true });
  await mkdir(rootB, { recursive: true });
  const projectA = requestedProject
    ? { id: requestedProject, name: "retrieval-disclosure", root: rootA, created_at: "0" } as Project
    : await cli<Project>({ type: "project_create", name: "retrieval-disclosure", root: rootA });
  const projectB = await cli<Project>({ type: "project_create", name: "retrieval-disclosure", root: rootB });
  assert.equal(projectB.name, "retrieval-disclosure", "project B carries the canonical title");
  if (!requestedProject) assert.equal(projectA.name, projectB.name, "both projects must share an identical title");
  assert.notEqual(projectA.id, projectB.id, "identical titles must still be distinct projects");

  const fixtureDir = join(temp, "fixtures");
  await mkdir(fixtureDir, { recursive: true });
  const fixturePath = join(fixtureDir, "workshop.md");
  await writeFile(fixturePath, new TextEncoder().encode("# Workshop\nworkshop body: the workshop notes are local only\n"));

  const revision = await cli<SourceRevision>({ type: "knowledge_ingest", project_id: projectA.id, source: { type: "file", path: fixturePath }, disclosure: { type: "local_only" } });
  assert(revision.locators > 0, "the ingested Markdown must publish content units");
  const citation: Citation = { source_id: revision.source_id, revision_digest: revision.digest, locator: { section: "Workshop" } };

  const inProject = await cli<CitationView>({ type: "knowledge_open_citation", project_id: projectA.id, citation });
  assert.equal(inProject.type, "content", "a source's own project must resolve its citation content");

  const foreign = await cli<CitationView>({ type: "knowledge_open_citation", project_id: projectB.id, citation });
  assert.equal(foreign.type, "missing", "an identically-titled foreign project must not resolve another project's citation");

  // Retrieval filters project + disclosure in SQL before ranking, but it also
  // requires a committed project policy — and no command commits one yet. The
  // honest outcome (hits, or the typed refusal) is recorded, never asserted.
  const retrieval = await call<CitedExcerpt[]>({ type: "knowledge_retrieve", project_id: projectA.id, query: "workshop", limit: 10 });
  const retrievalEvidence = retrieval.status === 0
    ? { outcome: "hits", count: retrieval.value!.length, disclosures: retrieval.value!.map((excerpt) => excerpt.disclosure.type) }
    : { outcome: "refused", code: retrieval.code, message: retrieval.message };

  const deletion = await cli<DeletionReceipt>({ type: "knowledge_delete_source", project_id: projectA.id, source_id: revision.source_id });

  const deletedInProject = await cli<CitationView>({ type: "knowledge_open_citation", project_id: projectA.id, citation });
  assert.equal(deletedInProject.type, "deleted", "a deleted source must resolve an explicit Deleted, never a substitution");

  const deletedForeign = await cli<CitationView>({ type: "knowledge_open_citation", project_id: projectB.id, citation });
  assert.equal(deletedForeign.type, "missing", "the foreign project still cannot resolve the citation after deletion");

  evidence.state = "passed";
  Object.assign(evidence, {
    project_a: projectA.id,
    project_b: projectB.id,
    source_id: revision.source_id,
    digest: revision.digest,
    locators: revision.locators,
    content_view: inProject.type,
    foreign_view: foreign.type,
    retrieval: retrievalEvidence,
    deletion,
    deleted_view: deletedInProject.type,
    deleted_foreign_view: deletedForeign.type,
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
