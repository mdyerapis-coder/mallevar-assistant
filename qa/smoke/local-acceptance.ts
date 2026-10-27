//! D5 local acceptance smoke: installs the real package under its own temporary
//! prefix, starts the daemon from the INSTALLED binary, drives it with the
//! INSTALLED CLI in isolated XDG sandboxes, and records the real per-step
//! result. It never touches the operator's install (`~/.local`), the running
//! operator daemon, or the operator store under `~/.local/share`.
//!
//! It asserts the real product behaviour end to end — a committed project
//! policy makes retrieval return the ingested source's own cited hit, the
//! knowledge FTS5 mirror survives `backup create` → `restore`, and the still-
//! honest refusals (`policy_missing`, `stale_policy_version`,
//! `illegal_transition`, `NamespaceExists`) fire — and never fabricates a
//! receipt, an artifact, a hit list or a "capability is available".
//!
//! Run: `bun qa/smoke/local-acceptance.ts` from the repository root.
import { strict as assert } from "node:assert";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Database } from "bun:sqlite";
import type { Subprocess } from "bun";
import type {
  BackupManifest,
  CitationView,
  CitedExcerpt,
  ImportPreview,
  ImportReceipt,
  MemoryRecord,
  Project,
  ProjectPolicy,
  RecoveryReport,
  RestoreReport,
  RunReceipt,
  SourceRevision,
} from "../../packages/contracts/src/generated";

const repoRoot = resolve(import.meta.dir, "..", "..");
const installScript = join(repoRoot, "packaging", "local-install.sh");
const reportDir = join(repoRoot, ".superpowers", "sdd", "2026-10-12-ironbark-assistant");
const reportPath = join(reportDir, "qa-smoke-local-acceptance-ts.json");

// ── receipt: every step records its exact command, exit code and real result ──
interface Step {
  name: string;
  command: string;
  exit_code: number;
  expected: string;
  observed: string;
}
const steps: Step[] = [];
const evidence: Record<string, unknown> = {};
const notes: string[] = [];

function record(name: string, command: string, exitCode: number, expected: string, observed: string): void {
  steps.push({ name, command, exit_code: exitCode, expected, observed });
}

// ── isolated XDG sandboxes; every spawned process shares exactly one ─────────
interface Sandbox extends NodeJS.ProcessEnv {
  HOME: string;
  XDG_DATA_HOME: string;
  XDG_STATE_HOME: string;
  XDG_RUNTIME_DIR: string;
}
function sandbox(root: string): Sandbox {
  return {
    ...process.env,
    HOME: root,
    XDG_DATA_HOME: join(root, "d"),
    XDG_STATE_HOME: join(root, "s"),
    XDG_RUNTIME_DIR: join(root, "r"),
  };
}
const dataDir = (env: Sandbox) => join(env.XDG_DATA_HOME, "ironbark-assistant");
const sha256 = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");

interface Outcome { code: number; stdout: string; stderr: string }

function deadline<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  const expired = Promise.withResolvers<never>();
  const timer = setTimeout(() => expired.reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  return Promise.race([promise, expired.promise]).finally(() => clearTimeout(timer));
}

async function exists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}

async function spawnCapture(binary: string, args: string[], env: Sandbox, timeoutMs = 60_000): Promise<Outcome> {
  const child = Bun.spawn([binary, ...args], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const [stdout, stderr, code] = await deadline(
    Promise.all([new Response(child.stdout).arrayBuffer(), new Response(child.stderr).text(), child.exited]),
    timeoutMs,
    `${binary} ${args.join(" ")}`,
  );
  return { code, stdout: Buffer.from(stdout).toString("utf8"), stderr };
}

// The INSTALLED CLI is the only client used against the sandboxes.
let cliBinary = "";
let coreBinary = "";
async function cli(env: Sandbox, args: string[], expect = 0): Promise<Outcome> {
  const outcome = await spawnCapture(cliBinary, args, env);
  assert.equal(outcome.code, expect, `\`${args.join(" ")}\` expected exit ${expect}, got ${outcome.code}: ${outcome.stderr.trim()}`);
  return outcome;
}
async function cliJson<T>(env: Sandbox, args: string[]): Promise<T> {
  return JSON.parse((await cli(env, args)).stdout.trim()) as T;
}
/// The CLI has no `knowledge`/`memory` subcommands: the full CoreCommand reaches
/// the daemon through the canonical `command` raw-JSON subcommand.
async function command<T>(env: Sandbox, payload: Record<string, unknown>): Promise<T> {
  return JSON.parse((await cli(env, ["command", "--request-id", crypto.randomUUID(), JSON.stringify(payload)])).stdout.trim()) as T;
}
async function commandRefusal(env: Sandbox, payload: Record<string, unknown>): Promise<Outcome> {
  const outcome = await spawnCapture(cliBinary, ["command", "--request-id", crypto.randomUUID(), JSON.stringify(payload)], env);
  assert.notEqual(outcome.code, 0, `${String(payload.type)} unexpectedly succeeded; expected a typed refusal`);
  return outcome;
}

// ── installed daemon lifecycle ───────────────────────────────────────────────
function readiness(stream: ReadableStream<Uint8Array>): Promise<string> {
  return deadline(
    (async () => {
      const decoder = new TextDecoder();
      let buffer = "";
      const reader = stream.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) throw new Error("installed core exited before reporting readiness");
        if (value) buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";
        for (const line of lines) if (line.includes("CORE_READY")) return line.trim();
      }
    })(),
    15_000,
    "installed core readiness",
  );
}
async function startInstalledCore(env: Sandbox, label: string): Promise<Subprocess<"ignore", "pipe", "pipe">> {
  for (const dir of [env.XDG_DATA_HOME, env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR]) await mkdir(dir, { recursive: true, mode: 0o700 });
  const child = Bun.spawn([coreBinary], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  try {
    evidence[`installed_core_ready:${label}`] = await readiness(child.stdout);
    return child;
  } catch (error) {
    child.kill("SIGKILL");
    await child.exited;
    throw error;
  }
}
async function stopCore(env: Sandbox, child: Subprocess<"ignore", "pipe", "pipe">): Promise<void> {
  await cli(env, ["shutdown"]);
  assert.equal(await deadline(child.exited, 15_000, "installed core shutdown"), 0);
}

// ── shared product sequences (genuine multi-step flows, reused per phase) ────
async function createProject(env: Sandbox, phase: string, name: string, root: string): Promise<string> {
  await mkdir(root, { recursive: true });
  const project = await cliJson<Project>(env, ["project", "create", name, "--root", root]);
  assert.ok(project.id, "project create must return an id");
  assert.equal(project.name, name);
  record(`${phase} project create`, `ironbark-cli project create ${name} --root ${root}`, 0, "a project DTO with an id", `project ${project.id}`);
  return project.id;
}

async function importAndReadArtifact(env: Sandbox, phase: string, projectId: string, descriptorPath: string, sourceBytes: Buffer): Promise<string> {
  const preview = await cliJson<ImportPreview>(env, ["import", "preview", "--project", projectId, "--file", descriptorPath]);
  assert.ok(preview.digest, "import preview must return a digest");
  assert.equal(preview.additive, true);
  record(`${phase} import preview`, `ironbark-cli import preview --project ${projectId} --file ${descriptorPath}`, 0, "an additive preview with a digest", `digest ${preview.digest}, files ${preview.files.length}`);

  const applied = await cliJson<ImportReceipt>(env, ["import", "apply", "--project", projectId, "--digest", preview.digest]);
  assert.ok(applied.artifact_id, "import apply must return an artifact id");
  assert.equal(applied.preview_digest, preview.digest);
  record(`${phase} import apply`, `ironbark-cli import apply --project ${projectId} --digest ${preview.digest}`, 0, "a receipt binding the preview digest to an artifact", `artifact ${applied.artifact_id}`);

  const artifact = await cli(env, ["artifact", "read", "--project", projectId, "--id", applied.artifact_id!]);
  assert.ok(artifact.stdout === sourceBytes.toString("utf8"), "artifact read bytes are not identical to the imported source");
  record(`${phase} artifact read`, `ironbark-cli artifact read --project ${projectId} --id ${applied.artifact_id}`, 0, "the artifact bytes, identical to the imported source", `bytes identical (sha256 ${sha256(sourceBytes)})`);
  return applied.artifact_id!;
}

async function startCheckpointedRun(env: Sandbox, phase: string, projectId: string, projectRoot: string, artifactId: string, manifestPath: string): Promise<string> {
  const run = await cliJson<RunReceipt>(env, ["run", "create", "--project", projectId, "--task", `${phase} queued work`, "--workspace", projectRoot]);
  assert.equal(run.state, "queued");
  record(`${phase} run create`, `ironbark-cli run create --project ${projectId} --task "${phase} queued work"`, 0, "a queued RunReceipt", `run ${run.run_id}, state ${run.state}`);

  const started = await cliJson<string>(env, ["run", "start", "--project", projectId, "--run", run.run_id, "--connection", "local", "--model", "local"]);
  assert.equal(started, "running");
  record(`${phase} run start`, `ironbark-cli run start --project ${projectId} --run ${run.run_id}`, 0, "the run transitions to running", `state ${started}`);

  await writeFile(manifestPath, JSON.stringify({ run_id: run.run_id, artifacts: [artifactId], checks: [], acceptance_satisfied: false, unresolved: ["queued work; not executed"] }));
  const checkpoint = await cliJson<string>(env, ["checkpoint", "capture", "--project", projectId, "--run", run.run_id, "--phase", "planned", "--manifest", manifestPath]);
  assert.ok(checkpoint.length > 0, "checkpoint capture must return an artifact id");
  record(`${phase} checkpoint capture`, `ironbark-cli checkpoint capture --project ${projectId} --run ${run.run_id} --phase planned`, 0, "a CAS manifest link artifact id", `artifact ${checkpoint}`);

  const inspection = await cliJson<RecoveryReport>(env, ["run", "inspect", "--project", projectId, "--run", run.run_id]);
  assert.equal(inspection.run_state, "running");
  assert.ok(inspection.checkpoint, "inspection must surface the captured checkpoint");
  record(`${phase} run inspect`, `ironbark-cli run inspect --project ${projectId} --run ${run.run_id}`, 0, "a RecoveryReport for the running run", `state ${inspection.run_state}, checkpoint ${inspection.checkpoint?.phase}`);
  return run.run_id;
}

/// Lists the content-addressed objects in a sandbox store (dot-files excluded).
async function listObjects(env: Sandbox): Promise<string[]> {
  try {
    return (await readdir(join(dataDir(env), "objects"))).filter((name) => !name.startsWith("."));
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw error;
  }
}

/// Counts the durable `source_revisions` rows for one project, read-only.
function revisionsInDb(dbPath: string, projectId: string): number {
  const db = new Database(dbPath, { readonly: true });
  try {
    const row = db.query("SELECT count(*) AS n FROM source_revisions r JOIN sources s ON s.id = r.source_id WHERE s.project_id = ?1").get(projectId) as { n: number } | null;
    return row?.n ?? 0;
  } finally {
    db.close();
  }
}

/// A shape the `PolicyService` accepts: local route allowed, bounded workers.
function localPolicy(projectId: string, version: string): ProjectPolicy {
  return {
    version,
    project_id: projectId,
    allowed_connection_ids: ["local"],
    allowed_tools: [],
    read_roots: [],
    write_roots: [],
    network_destinations: [],
    local_only: false,
    max_workers: 2,
    max_requests: 10,
    spend_policy: "free_only",
    allow_unknown_cost: false,
  };
}

// ── operator paths: read-only mtime snapshot, never written ──────────────────
interface Snapshot { path: string; mtimeMs: number }
async function snapshot(paths: string[]): Promise<Snapshot[]> {
  const out: Snapshot[] = [];
  for (const path of paths) {
    try {
      const info = await stat(path);
      out.push({ path, mtimeMs: info.mtimeMs });
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
  }
  return out;
}

const operatorHome = process.env.HOME ?? "";
const operatorInstall = [
  join(operatorHome, ".local", "bin", "ironbark-core"),
  join(operatorHome, ".local", "bin", "ironbark-cli"),
  join(operatorHome, ".local", "share", "ironbark-assistant", "installed-manifest.json"),
];

let temp: string | undefined;
const running: Subprocess<"ignore", "pipe", "pipe">[] = [];

try {
  temp = await mkdtemp(join(tmpdir(), "ibd5-"));
  const prefix = join(temp, "prefix");
  const envA = sandbox(join(temp, "sandbox-a"));
  const envB = sandbox(join(temp, "sandbox-b"));
  const envF = sandbox(join(temp, "sandbox-file"));
  for (const env of [envA, envB, envF]) {
    assert.ok(env.HOME.startsWith(temp), "sandbox HOME must live under the temporary root");
    assert.ok(env.XDG_DATA_HOME.startsWith(temp), "sandbox XDG_DATA_HOME must live under the temporary root");
  }
  assert.notEqual(prefix, join(operatorHome, ".local"), "the smoke prefix must not be the operator install");
  evidence.temp_root = temp;
  evidence.install_prefix = prefix;

  const operatorBefore = await snapshot(operatorInstall);
  evidence.operator_install_before = operatorBefore;

  // ── 1. install, then --check against the tree ──────────────────────────────
  {
    const outcome = await spawnCapture("bash", [installScript, "--prefix", prefix], envA, 900_000);
    record("install", `bash packaging/local-install.sh --prefix ${prefix}`, outcome.code, "exit 0", outcome.code === 0 ? "package installed under the temporary prefix" : outcome.stderr.trim());
    assert.equal(outcome.code, 0, `local-install.sh failed: ${outcome.stderr.trim()}`);
    coreBinary = join(prefix, "bin", "ironbark-core");
    cliBinary = join(prefix, "bin", "ironbark-cli");
    assert.ok(await exists(coreBinary), "install did not produce the installed core binary");
    assert.ok(await exists(cliBinary), "install did not produce the installed CLI binary");
  }
  {
    const outcome = await spawnCapture("bash", [installScript, "--prefix", prefix, "--check"], envA, 300_000);
    const matched = outcome.code === 0 && outcome.stdout.includes("Installed copies match the current tree.");
    record("install --check", `bash packaging/local-install.sh --prefix ${prefix} --check`, outcome.code, "exit 0 and 'Installed copies match the current tree.'", matched ? "install matches the tree" : outcome.stdout.trim());
    assert.ok(matched, `--check did not confirm the install matches the tree:\n${outcome.stdout}\n${outcome.stderr}`);
  }

  // ═══ Phase 0: a File ingest publishes its bytes into the CAS, so the revision
  //     digest is a resolvable retention root and `backup create` succeeds.
  //     Mirrors `file_ingest_publishes_the_source_bytes_into_the_cas`. ═══
  running.push(await startInstalledCore(envF, "file-probe"));
  const fileProjectId = await createProject(envF, "file-ingest", "d5-file-ingest", join(temp, "file-probe-root"));
  const sourceFileBytes = Buffer.from("cited bytes must live in the content-addressed store\n");
  const probePath = join(temp, "file-ingest.txt");
  await writeFile(probePath, sourceFileBytes);
  const fileRevision = await command<SourceRevision>(envF, { type: "knowledge_ingest", project_id: fileProjectId, source: { type: "file", path: probePath }, disclosure: { type: "local_only" } });
  assert.ok(fileRevision.locators > 0, "the File ingest must publish a revision");
  const objectPath = join(dataDir(envF), "objects", fileRevision.digest);
  assert.ok(await exists(objectPath), "the File ingest must publish the revision digest into the CAS");
  const publishedBytes = await readFile(objectPath);
  assert.ok(publishedBytes.equals(sourceFileBytes), "the CAS object named by the revision digest must be byte-identical to the source file");
  const objectsAfterFirst = await listObjects(envF);
  assert.equal(objectsAfterFirst.length, 1, "the File ingest must publish exactly one content-addressed object");
  record("file-ingest probe: knowledge ingest publishes the CAS object", `ironbark-cli command knowledge_ingest file (${probePath})`, 0, "the revision digest is a resolvable CAS object with byte-identical content", `digest ${fileRevision.digest.slice(0, 12)}…, bytes ${publishedBytes.length}, CAS objects ${objectsAfterFirst.length}`);

  // Re-ingesting identical bytes is idempotent: one revision, one object.
  const fileRevisionAgain = await command<SourceRevision>(envF, { type: "knowledge_ingest", project_id: fileProjectId, source: { type: "file", path: probePath }, disclosure: { type: "local_only" } });
  assert.equal(fileRevisionAgain.source_id, fileRevision.source_id, "identical bytes must resolve to the same source");
  assert.equal(fileRevisionAgain.digest, fileRevision.digest, "identical bytes must resolve to one revision");
  assert.equal((await listObjects(envF)).length, 1, "an idempotent publish must not duplicate or replace the object");
  const fileRevisionRows = revisionsInDb(join(dataDir(envF), "core.sqlite3"), fileProjectId);
  assert.equal(fileRevisionRows, 1, "identical bytes must leave exactly one source_revisions row");
  record("file-ingest probe: idempotent re-ingest", `ironbark-cli command knowledge_ingest file (${probePath}) again`, 0, "one revision, one object", `revision ${fileRevisionAgain.source_id} unchanged, CAS objects 1, source_revisions rows ${fileRevisionRows}`);

  const probeBackupDir = join(temp, "file-probe-backup");
  const probeBackup = await cliJson<BackupManifest>(envF, ["backup", "create", "--destination", probeBackupDir]);
  assert.equal(probeBackup.format, 1);
  assert.ok(probeBackup.objects.some((object) => object.digest === fileRevision.digest), "the File-ingest revision must be exported as its own object closure");
  record("file-ingest probe: backup create", `ironbark-cli backup create --destination ${probeBackupDir}`, 0, "the File-ingest revision root is exported", `format ${probeBackup.format}, objects ${probeBackup.objects.length}`);
  notes.push("A File ingest now publishes the resolved bytes into the CAS (`IngestCasPublish`), so `source_revisions.digest` is a resolvable retention root: the CAS holds one object whose digest is the revision digest and whose bytes are the source file, re-ingestion is idempotent (one revision, one object), and `backup create` succeeds.");
  await stopCore(envF, running.pop()!);

  evidence.file_ingest = { revision_digest: fileRevision.digest, source_id: fileRevision.source_id, cas_objects: objectsAfterFirst.length, source_revisions_rows: fileRevisionRows };

  // ═══════════════════ Phase A: the full product surface ═════════════════════
  running.push(await startInstalledCore(envA, "source"));
  const projectRoot = join(temp, "project-root");
  const projectId = await createProject(envA, "phase A", "d5-local-acceptance", projectRoot);

  // The ingested source is imported first so its bytes live in the CAS: the
  // revision digest then equals the exported artifact digest, so the object
  // closure the exporter verifies is complete.
  const markdownBytes = Buffer.from("# Workshop\nthe workshop notes are local only\n");
  const markdownPath = join(temp, "workshop.md");
  await writeFile(markdownPath, markdownBytes);
  const descriptorPath = join(temp, "import-descriptor.json");
  await writeFile(descriptorPath, JSON.stringify({ kind: "source", uri: "file:workshop.md", path: "workshop.md" }));
  const artifactId = await importAndReadArtifact(envA, "phase A", projectId, descriptorPath, markdownBytes);
  evidence.project_id = projectId;
  evidence.imported_artifact_id = artifactId;
  evidence.imported_artifact_sha256 = sha256(markdownBytes);

  // A committed project policy: without it retrieval is refused.
  const policyPath = join(temp, "policy-v1.json");
  await writeFile(policyPath, JSON.stringify(localPolicy(projectId, "1")));
  const committed = await cliJson<ProjectPolicy>(envA, ["policy", "set", "--project", projectId, "--file", policyPath]);
  assert.equal(committed.version, 1);
  assert.equal(committed.project_id, projectId);
  record("policy set", `ironbark-cli policy set --project ${projectId} --file ${policyPath}`, 0, "the committed ProjectPolicy (version 1)", `version ${committed.version}, local_only ${committed.local_only}, allowed_connections ${committed.allowed_connection_ids.join(",")}`);

  const shown = await cliJson<ProjectPolicy>(envA, ["policy", "show", "--project", projectId]);
  assert.equal(shown.version, 1);
  assert.deepEqual(shown, committed);
  record("policy show (round-trip)", `ironbark-cli policy show --project ${projectId}`, 0, "the latest committed policy, equal to the committed one", `version ${shown.version} matches the committed policy`);

  // Equal / stale version is refused; a strictly greater version commits.
  const stalePath = join(temp, "policy-v1-stale.json");
  await writeFile(stalePath, JSON.stringify(localPolicy(projectId, "1")));
  const stale = await spawnCapture(cliBinary, ["policy", "set", "--project", projectId, "--file", stalePath], envA);
  assert.notEqual(stale.code, 0, "re-committing an equal version must be refused");
  assert.match(stale.stderr, /stale_policy_version/, `expected stale_policy_version, got: ${stale.stderr.trim()}`);
  record("policy set (equal version)", `ironbark-cli policy set --project ${projectId} --file ${stalePath}`, stale.code, "typed refusal: an equal version is stale", stale.stderr.trim().split("\n")[0]);

  const policyV2Path = join(temp, "policy-v2.json");
  await writeFile(policyV2Path, JSON.stringify(localPolicy(projectId, "2")));
  const committedV2 = await cliJson<ProjectPolicy>(envA, ["policy", "set", "--project", projectId, "--file", policyV2Path]);
  assert.equal(committedV2.version, 2);
  record("policy set (version 2)", `ironbark-cli policy set --project ${projectId} --file ${policyV2Path}`, 0, "version 2 commits past the refused stale set", `version ${committedV2.version}`);

  // ── knowledge ingest (from the imported artifact) → retrieve → citation ────
  const revision = await command<SourceRevision>(envA, { type: "knowledge_ingest", project_id: projectId, source: { type: "attachment", artifact_id: artifactId }, disclosure: { type: "local_only" } });
  assert.ok(revision.locators > 0, "a published revision must carry content units");
  assert.equal(revision.digest, sha256(markdownBytes), "the revision digest must be the ingested content digest (the exported artifact)");
  record("knowledge ingest", "ironbark-cli command knowledge_ingest (attachment: imported artifact)", 0, "a SourceRevision whose digest is the exported artifact digest", `source ${revision.source_id}, mime ${revision.mime}, locators ${revision.locators}, digest ${revision.digest.slice(0, 12)}…`);
  evidence.source_revision = { source_id: revision.source_id, digest: revision.digest, locators: revision.locators, mime: revision.mime };

  // Retrieval now returns the ingested source's own cited hit.
  const hits = await command<CitedExcerpt[]>(envA, { type: "knowledge_retrieve", project_id: projectId, query: "workshop", limit: 5 });
  assert.ok(hits.length >= 1, "a committed policy must make retrieval return the ingested source's chunk");
  const hit = hits[0];
  assert.equal(hit.citation.source_id, revision.source_id, "the hit must cite the ingested source");
  assert.equal(hit.citation.revision_digest, revision.digest, "the hit's citation digest must match the ingested revision");
  assert.match(hit.text, /workshop/i, "the hit text must contain the queried term");
  record("knowledge retrieve", "ironbark-cli command knowledge_retrieve (workshop)", 0, "a real cited hit for the ingested source", `hits ${hits.length}, source ${hit.citation.source_id}, revision_digest ${hit.citation.revision_digest.slice(0, 12)}…, text ${JSON.stringify(hit.text.slice(0, 48))}`);
  evidence.retrieval_hit = { hits: hits.length, source_id: hit.citation.source_id, revision_digest: hit.citation.revision_digest, locator: hit.citation.locator, disclosure: hit.disclosure.type };

  // A second project with no committed policy still refuses retrieval honestly.
  const bareProjectId = await createProject(envA, "phase A", "d5-no-policy", join(temp, "no-policy-root"));
  const noPolicyRefusal = await commandRefusal(envA, { type: "knowledge_retrieve", project_id: bareProjectId, query: "workshop", limit: 5 });
  assert.match(noPolicyRefusal.stderr, /policy_missing/, `a policy-less project must refuse retrieval with policy_missing, got: ${noPolicyRefusal.stderr.trim()}`);
  record("knowledge retrieve (no policy)", `ironbark-cli command knowledge_retrieve --project ${bareProjectId}`, noPolicyRefusal.code, "typed refusal: the project has no committed policy", noPolicyRefusal.stderr.trim().split("\n")[0]);

  // The retrieved citation resolves to its exact stored content.
  const opened = await command<CitationView>(envA, { type: "knowledge_open_citation", project_id: projectId, citation: hit.citation });
  assert.equal(opened.type, "content", "the retrieved citation must resolve to its exact stored content");
  record("citation open", "ironbark-cli command knowledge_open_citation (retrieved citation)", 0, "CitationView::Content for the source's own project", `content: ${JSON.stringify(opened.text.slice(0, 48))}`);
  evidence.citation_open = { type: opened.type, text: opened.text };

  // ── memory propose → approve → retrieve → forget ───────────────────────────
  const candidateId = await command<string>(envA, { type: "memory_propose", project_id: projectId, text: "the workshop torque spec is 40 Nm", source: "d5 smoke" });
  assert.ok(candidateId.length > 0, "the core must assign a candidate id");
  const beforeApproval = await command<MemoryRecord[]>(envA, { type: "knowledge_search_memory", project_id: projectId, query: "torque", limit: 10 });
  assert.equal(beforeApproval.length, 0, "a proposed memory must not be retrievable before approval");
  record("memory propose", "ironbark-cli command memory_propose", 0, "a candidate id, not retrievable yet", `candidate ${candidateId}, retrievable before approval: 0`);

  const approved = await command<MemoryRecord>(envA, { type: "memory_decide", project_id: projectId, candidate_id: candidateId, decision: "approve" });
  assert.equal(approved.state, "approved");
  record("memory approve", "ironbark-cli command memory_decide approve", 0, "the record enters the approved state", `record ${approved.id}, state ${approved.state}`);

  const afterApproval = await command<MemoryRecord[]>(envA, { type: "knowledge_search_memory", project_id: projectId, query: "torque", limit: 10 });
  assert.equal(afterApproval.length, 1, "an approved memory must be retrievable");
  assert.equal(afterApproval[0].id, approved.id);
  record("memory retrieve", "ironbark-cli command knowledge_search_memory", 0, "the approved memory is retrievable", `${afterApproval.length} record(s), id ${afterApproval[0].id}`);

  const forgotten = await command<MemoryRecord>(envA, { type: "memory_forget", project_id: projectId, record_id: approved.id });
  assert.equal(forgotten.state, "forgotten");
  const gone = await command<MemoryRecord[]>(envA, { type: "knowledge_search_memory", project_id: projectId, query: "torque", limit: 10 });
  assert.equal(gone.length, 0, "a forgotten memory must not be retrievable");
  record("memory forget", "ironbark-cli command memory_forget", 0, "the record is marked forgotten and drops out of retrieval", `state ${forgotten.state}, retrievable after forget: 0`);

  // ── truthful capability report: nothing is qualified silently ──────────────
  const qualification = await command<{ enabled: unknown; blocked: unknown[] }>(envA, { type: "judgment_qualification_list", domain: "retrieval_relevance" });
  assert.equal(qualification.enabled, null, "no judgment domain may be reported qualified without a recorded qualification");
  record("capability report", "ironbark-cli command judgment_qualification_list retrieval_relevance", 0, "no domain qualified (enabled: null)", `enabled ${JSON.stringify(qualification.enabled)}, blocked ${qualification.blocked.length}`);
  evidence.qualification = qualification;

  // ── run create → start → checkpoint capture → inspect → reconcile ──────────
  const runId = await startCheckpointedRun(envA, "phase A", projectId, projectRoot, artifactId, join(temp, "phase-a-checkpoint.json"));

  // Reconcile requires a crashed (blocked) run; on a running run the state
  // machine refuses it. The typed refusal is the real result here.
  const reconcileRefusal = await spawnCapture(cliBinary, ["run", "reconcile", "--project", projectId, "--run", runId, "--decision", "abandon"], envA);
  assert.notEqual(reconcileRefusal.code, 0, "reconciling a running run must be refused");
  assert.match(reconcileRefusal.stderr, /illegal_transition/, `expected illegal_transition, got: ${reconcileRefusal.stderr.trim()}`);
  record("run reconcile (running)", `ironbark-cli run reconcile --project ${projectId} --run ${runId} --decision abandon`, reconcileRefusal.code, "typed refusal: a running run cannot be reconciled", reconcileRefusal.stderr.trim().split("\n")[0]);

  // ── backup create AFTER the knowledge ingest: the exporter now reviews the
  //    FTS5 mirror, so this succeeds where it used to fail.
  const backupDir = join(temp, "backup");
  const backup = await cliJson<BackupManifest>(envA, ["backup", "create", "--destination", backupDir]);
  assert.equal(backup.format, 1);
  assert.ok(backup.objects.some((object) => object.digest === sha256(markdownBytes)), "the backup must export the ingested source's object closure");
  record("backup create (after ingest)", `ironbark-cli backup create --destination ${backupDir}`, 0, "a BackupManifest including the ingested source and the knowledge FTS mirror", `format ${backup.format}, objects ${backup.objects.length}, database ${backup.database_digest.slice(0, 12)}…`);
  evidence.backup = { objects: backup.objects.length, database_digest: backup.database_digest };

  await stopCore(envA, running.pop()!);

  // ═══════ Phase B: offline restore, then retrieval returns the same hit ═════
  const restore = await cliJson<RestoreReport>(envB, ["backup", "restore", "--source", backupDir, "--new-namespace"]);
  assert.equal(restore.stopped, true);
  assert.ok(BigInt(restore.blocked_runs) >= 1n, "the restore must block the in-flight run rather than replay it");
  record("backup restore --new-namespace", `ironbark-cli backup restore --source ${backupDir} --new-namespace`, 0, "an offline RestoreReport into a fresh namespace", `stopped ${restore.stopped}, blocked_runs ${restore.blocked_runs}`);

  running.push(await startInstalledCore(envB, "restored"));

  // The restored namespace keeps the committed policy and the FTS index, so the
  // same retrieval hit comes back.
  const restoredHits = await command<CitedExcerpt[]>(envB, { type: "knowledge_retrieve", project_id: projectId, query: "workshop", limit: 5 });
  assert.ok(restoredHits.length >= 1, "retrieval after restore must still return the ingested source's chunk");
  assert.equal(restoredHits[0].citation.source_id, revision.source_id, "the restored hit must cite the same source");
  assert.equal(restoredHits[0].citation.revision_digest, revision.digest, "the restored hit's revision digest must match the ingested revision");
  record("knowledge retrieve (restored)", "ironbark-cli command knowledge_retrieve (workshop)", 0, "the same cited hit after an offline restore", `hits ${restoredHits.length}, revision_digest ${restoredHits[0].citation.revision_digest.slice(0, 12)}…`);
  evidence.restored_retrieval_hit = { hits: restoredHits.length, source_id: restoredHits[0].citation.source_id, revision_digest: restoredHits[0].citation.revision_digest };

  const restoredRun = await cliJson<RecoveryReport>(envB, ["run", "inspect", "--project", projectId, "--run", runId]);
  assert.equal(restoredRun.run_state, "blocked", "the restored run must be blocked, not silently resumed");
  record("run inspect (restored)", `ironbark-cli run inspect --project ${projectId} --run ${runId}`, 0, "the restored run reports blocked", `state ${restoredRun.run_state}`);

  const reconciled = await cli(envB, ["run", "reconcile", "--project", projectId, "--run", runId, "--decision", "abandon"]);
  const afterReconcile = await cliJson<RecoveryReport>(envB, ["run", "inspect", "--project", projectId, "--run", runId]);
  assert.equal(afterReconcile.run_state, "cancelled", "abandoning a blocked run must reach a terminal state");
  record("run reconcile (blocked → abandon)", `ironbark-cli run reconcile --project ${projectId} --run ${runId} --decision abandon`, reconciled.code, "the blocked run is abandoned to a terminal state", `state ${afterReconcile.run_state}`);

  // ── artifact bytes survive the restore, identically ────────────────────────
  const restoredArtifact = await cli(envB, ["artifact", "read", "--project", projectId, "--id", artifactId]);
  assert.ok(restoredArtifact.stdout === markdownBytes.toString("utf8"), "the artifact bytes after restore must be identical to the imported source");
  record("artifact read (restored)", `ironbark-cli artifact read --project ${projectId} --id ${artifactId}`, 0, "the same artifact bytes after an offline restore", `bytes identical (sha256 ${sha256(markdownBytes)})`);
  evidence.restored_artifact_sha256 = sha256(markdownBytes);

  // ── a second restore into the same namespace stays refused ─────────────────
  const secondRestore = await spawnCapture(cliBinary, ["backup", "restore", "--source", backupDir, "--new-namespace"], envB);
  assert.notEqual(secondRestore.code, 0, "restoring over an existing namespace must be refused");
  assert.match(secondRestore.stderr, /NamespaceExists/, `expected NamespaceExists, got: ${secondRestore.stderr.trim()}`);
  record("second restore (same namespace)", `ironbark-cli backup restore --source ${backupDir} --new-namespace`, secondRestore.code, "refused with NamespaceExists", secondRestore.stderr.trim().split("\n")[0]);

  // ── doctor: sqlite integrity ok with foreign keys on ───────────────────────
  const doctor = await cliJson<{ sqlite_integrity: string; foreign_keys: boolean; schema_version: number }>(envB, ["doctor", "--json"]);
  assert.equal(doctor.sqlite_integrity, "ok", "doctor must report a healthy database integrity check");
  assert.equal(doctor.foreign_keys, true, "doctor must report enforced foreign keys");
  record("doctor", "ironbark-cli doctor --json", 0, "sqlite_integrity: ok with foreign_keys: true", `sqlite_integrity ${doctor.sqlite_integrity}, foreign_keys ${doctor.foreign_keys}, schema_version ${doctor.schema_version}`);
  evidence.doctor = doctor;

  await stopCore(envB, running.pop()!);

  // ── uninstall: installed files gone, runtime state left alone ──────────────
  assert.ok(await exists(join(dataDir(envA), "core.sqlite3")), "the sandbox store should exist before uninstall");
  {
    const outcome = await spawnCapture("bash", [installScript, "--prefix", prefix, "--uninstall"], envA, 120_000);
    record("uninstall", `bash packaging/local-install.sh --prefix ${prefix} --uninstall`, outcome.code, "exit 0, installed files removed", outcome.code === 0 ? "uninstall completed" : outcome.stderr.trim());
    assert.equal(outcome.code, 0, `uninstall failed: ${outcome.stderr.trim()}`);
  }
  const coreGone = !(await exists(coreBinary));
  const cliGone = !(await exists(cliBinary));
  const manifestGone = !(await exists(join(prefix, "share", "ironbark-assistant", "installed-manifest.json")));
  const runtimeKept = await exists(join(dataDir(envA), "core.sqlite3"));
  assert.ok(coreGone && cliGone, "uninstall must remove the installed binaries");
  assert.ok(manifestGone, "uninstall must remove the install manifest");
  assert.ok(runtimeKept, "uninstall must leave the XDG runtime state untouched");
  record("uninstall leaves runtime state", `test -f ${join(dataDir(envA), "core.sqlite3")}`, 0, "installed files gone, runtime state preserved", `core removed: ${coreGone}, cli removed: ${cliGone}, manifest removed: ${manifestGone}, runtime store kept: ${runtimeKept}`);

  // ── the operator install was never touched ─────────────────────────────────
  const operatorAfter = await snapshot(operatorInstall);
  assert.deepEqual(operatorAfter, operatorBefore, "the operator install under ~/.local must be byte-for-byte untouched");
  record("operator install untouched", `stat ${operatorInstall.join(" ")}`, 0, "operator install mtimes unchanged", `unchanged paths: ${operatorBefore.map((entry) => entry.path).join(", ") || "(none present)"}`);

  evidence.state = "passed";
} catch (error) {
  evidence.state = "failed";
  evidence.error = error instanceof Error ? error.message : String(error);
  process.exitCode = 1;
} finally {
  for (const child of running) {
    if (child.exitCode === null) {
      child.kill("SIGINT");
      try {
        await deadline(child.exited, 5_000, "core stop");
      } catch {
        child.kill("SIGKILL");
        await child.exited;
      }
    }
  }
  if (temp) await rm(temp, { recursive: true, force: true });

  const receipt = {
    state: evidence.state ?? "failed",
    command: "bun qa/smoke/local-acceptance.ts",
    mechanism: "the real packaging/local-install.sh installs the release package under a temporary prefix; the INSTALLED core daemon and INSTALLED CLI run in isolated XDG sandboxes; every result is the daemon's own reply",
    design_acceptance_doc: "qa/smoke/e2e-acceptance.md",
    cli_surface_observed: ["project", "policy", "import", "artifact", "backup", "retention", "object-capacity", "conversation", "run", "checkpoint", "status", "events", "doctor", "shutdown", "command"],
    cli_surface_note: "There are no `knowledge`/`memory` subcommands; the full CoreCommand (knowledge_ingest/retrieve/open_citation, memory_propose/decide/forget, knowledge_search_memory, judgment_qualification_list) reaches the daemon through the canonical `command` raw-JSON subcommand. `policy set`/`policy show` are real CLI subcommands.",
    isolation: "every spawned process received its own HOME/XDG_DATA_HOME/XDG_STATE_HOME/XDG_RUNTIME_DIR under one temporary root; the operator install (~/.local), daemon and store were only stat()ed, never written",
    not_run_owner_approval: [
      "live provider/harness turns — a real model turn through a harness adapter needs an owner-approved credentialed provider route and network; not reachable offline and not claimed",
      "JEV evaluation — judgment qualification / advisory evaluation against a calibrated holdout needs owner-approved evaluation runs; only the (read-only) qualification registry was queried, and it truthfully reports no qualified domain",
      "the desktop surface — the Tauri ironbark-desktop GUI needs an interactive desktop session and is not headless-verifiable from this smoke",
    ],
    steps,
    evidence,
    notes,
  };
  await mkdir(reportDir, { recursive: true });
  await writeFile(reportPath, `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(JSON.stringify({ state: receipt.state, steps: steps.map((step) => `${step.name}: exit ${step.exit_code} — ${step.observed}`), receipt: reportPath, error: evidence.error }, null, 2));
}
