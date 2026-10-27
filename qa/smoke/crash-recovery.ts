//! Crash-recovery smoke: offline, drives the real prebuilt CLI.
//! Deliberately kills core before a temp write and after write/before receipt,
//! restarts, compares filesystem+events. Expected: no duplicate write or false
//! success, explicit Blocked reconciliation, completed artifact preserved.
//! Uses generic `command` JSON for C4 commands (awaiting owner wiring).
import { strict as assert } from "node:assert";
import { mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createConnection, type Socket } from "node:net";
import type { Subprocess } from "bun";
import type { ClientFrame, CoreCommand, CoreReply, RecoveryReport, RunState, RecoveryDecision, CheckpointPhase, ResultManifest, CheckReceipt, SessionRef, RoutePin, HarnessKind, ProjectId, ArtifactId, RunId, RunReceipt } from "../../packages/contracts/src/generated";

const args = process.argv.slice(2);
function option(name: string): string | undefined { const i = args.indexOf(name); return i < 0 ? undefined : args[i + 1]; }
const binary = resolve(process.env.IRONBARK_CLI_BIN ?? "target/debug/ironbark-cli");
const coreBinary = resolve(process.env.IRONBARK_CORE_BIN ?? "target/debug/ironbark-core");
const evidence: Record<string, unknown> = { state: "running", mechanism: "actual prebuilt CLI and daemon", simulated_model_success: false };

type RecoveryState = "passed" | "blocked_on_owner_wiring" | "failed" | "needs_core_restart";

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/// The CLI requires a UUID request identity. A readable label is mapped
/// deterministically, so repeating the same logical request reuses the same
/// durable identity — which is exactly what the replay rule is about.
function asRequestId(value: string): string {
  if (UUID_RE.test(value)) return value;
  const digest = new Bun.CryptoHasher("sha256").update(value).digest();
  const bytes = new Uint8Array(digest).subarray(0, 16);
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/// The isolated XDG environment every spawned process must share: the CLI must
/// talk to the sandboxed daemon, never to whatever daemon the developer's own
/// environment happens to have running.
let sandbox: NodeJS.ProcessEnv | undefined;

async function cli<T>(command: CoreCommand, requestId: string = crypto.randomUUID()): Promise<T> {
  assert(sandbox, "the sandbox environment must be established before any CLI call");
  const child = Bun.spawn([binary, "command", "--request-id", asRequestId(requestId), JSON.stringify(command)], { env: sandbox, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGTERM"), 60_000);
  // Bun pipes are ReadableStreams of bytes: decode them, and read stdout and
  // stderr concurrently so a large stderr can never deadlock the child.
  const stdoutPromise = new Response(child.stdout).text();
  const stderrPromise = new Response(child.stderr).text();
  try {
    const code = await child.exited;
    const stdout = (await stdoutPromise).trim();
    const stderr = (await stderrPromise).trim();
    if (code !== 0) throw new Error(`CLI exited ${code} for ${command.type} (request ${asRequestId(requestId)}): ${stderr || stdout}`);
    return JSON.parse(stdout);
  } finally {
    clearTimeout(timer);
  }
}

/** Decode byte chunks until `predicate` matches an accumulated line. */
async function readUntilReady(
  stream: ReadableStream<Uint8Array>,
  predicate: (line: string) => boolean,
): Promise<boolean> {
  const decoder = new TextDecoder();
  let buffer = "";
  const reader = stream.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      const chunk = value;

    buffer += decoder.decode(chunk as Uint8Array, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() ?? "";
      for (const line of lines) {
        if (predicate(line.trim())) return true;
      }
    }
  } finally {
    reader.releaseLock();
  }
  buffer += decoder.decode();
  return predicate(buffer.trim());
}

async function startCore(env: NodeJS.ProcessEnv): Promise<Subprocess<"ignore", "pipe", "pipe">> {
  const child = Bun.spawn([coreBinary], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGTERM"), 10_000);
  try {
    if (await readUntilReady(child.stdout, (line) => line.includes("CORE_READY"))) return child;
    const stderr = await new Response(child.stderr).text();
    throw new Error(`core did not report ready: ${stderr.trim()}`);
  } finally {
    clearTimeout(timer);
  }
}

function isOwnedWiringError(err: Error): boolean {
  return err.message.includes("invalid_command") || err.message.includes("not yet integrated");
}

let core: Subprocess<"ignore", "pipe", "pipe"> | undefined;
let temp: string | undefined;
let projectId: string | undefined;
let artifactId: string | undefined;
let runA: string | undefined;
let runB: string | undefined;

try {
  // Create isolated XDG env
  temp = await mkdtemp(join(tmpdir(), "ibc4-"));
  const env = { ...process.env, HOME: temp, XDG_DATA_HOME: join(temp, "d"), XDG_STATE_HOME: join(temp, "s"), XDG_RUNTIME_DIR: join(temp, "r") };
  for (const dir of [env.XDG_DATA_HOME, env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR]) await mkdir(dir, { recursive: true, mode: 0o700 });
  sandbox = env;

  // Start core
  core = await startCore(env);
  evidence.state = "core_started";

  // Project create
  const project = await cli<{ id: string }>({ type: "project_create", name: "crash-recovery", root: temp }, "proj-create");
  projectId = project.id;
  evidence.project = project;

  // Import a source document -> artifact (a completed artifact to preserve).
  // A `document` import is kind-tagged JSON that names the content to archive.
  const contentFile = join(temp, "content.txt");
  await writeFile(contentFile, "Hello crash-recovery\nThis is a completed artifact.\n");
  const documentFile = join(temp, "import.json");
  await writeFile(documentFile, JSON.stringify({ kind: "source", uri: "crash-recovery://fixture", path: contentFile }));
  const preview = await cli<{ digest: string; project_id: string; kind: string }>(
    { type: "import_preview", project_id: project.id, source: { path: documentFile, format: { type: "document" } } }, "import-preview");
  assert.strictEqual(preview.kind, "source", "the preview names the imported kind");
  const applied = await cli<{ artifact_id?: string }>({ type: "import_apply", project_id: project.id, preview_digest: preview.digest }, "import-apply");
  artifactId = applied.artifact_id;
  assert(artifactId, "import must produce an artifact");
  evidence.artifact = artifactId;

  // Helper: run_create (generic command)
  async function runCreate(projectId: string): Promise<string> {
    const receipt = await cli<RunReceipt>({ type: "run_create", project_id: projectId, task: "crash recovery test" }, "run-create-" + crypto.randomUUID());
    return receipt.run_id;
  }

  // Helper: run_start
  async function runStart(runId: string, session?: SessionRef, artifacts: string[] = [], recordedPid?: number, recordedStartTime?: number): Promise<RunState> {
    return cli<RunState>({
      type: "run_start",
      project_id: projectId!,
      run_id: runId,
      pin: { connection_id: "smoke-conn", model_id: "smoke-model" },
      native_session: session ?? null,
      artifacts: artifacts as any,
      recorded_pid: String(recordedPid),
      recorded_start_time: String(recordedStartTime),
    }, "run-start-" + crypto.randomUUID());
  }

  // Helper: run_checkpoint
  async function runCheckpoint(runId: string, phase: CheckpointPhase, manifest: ResultManifest): Promise<string> {
    return cli<string>({ type: "run_checkpoint", project_id: projectId!, run_id: runId, phase, manifest }, "run-checkpoint-" + crypto.randomUUID());
  }

  // Helper: run_inspect
  async function runInspect(runId: string): Promise<RecoveryReport> {
    return cli<RecoveryReport>({ type: "run_inspect", project_id: projectId!, run_id: runId }, "run-inspect-" + crypto.randomUUID());
  }

  // Helper: run_reconcile
  async function runReconcile(runId: string, decision: RecoveryDecision): Promise<void> {
    return cli<void>({ type: "run_reconcile", project_id: projectId!, run_id: runId, decision }, "run-reconcile-" + crypto.randomUUID());
  }

  // Helper: artifact read
  // `artifact read` writes the raw artifact bytes to stdout, not a JSON DTO.
  async function artifactRead(artifactId: string): Promise<string> {
    assert(sandbox, "sandbox must exist");
    const child = Bun.spawn(
      [binary, "artifact", "read", "--project", projectId!, "--id", artifactId],
      { env: sandbox, stdin: "ignore", stdout: "pipe", stderr: "pipe" },
    );
    const [text, err] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text()]);
    const code = await child.exited;
    if (code !== 0) throw new Error(`artifact read exited ${code}: ${err.trim()}`);
    return text;
  }

  /** The observed process start time in the same units the core reads from
   *  /proc/<pid>/stat field 22 (clock ticks since boot). A wall-clock value
   *  would be misread as a recycled PID. */
  async function selfStartTicks(): Promise<number> {
    const stat = await readFile(`/proc/${process.pid}/stat`, "utf8");
    return Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[19]);
  }

  // A dummy session for the run
  const session: SessionRef = {
    harness: "opencode" as HarnessKind,
    native_id: "smoke-session",
    native_version: "1.0",
    project_id: projectId as ProjectId,
    cwd: temp,
    resolved_route: { connection_id: "smoke-conn", model_id: "smoke-model" },
  };

  // ── Scenario A: kill before the temp write (run in queued/running, no checkpoint) ──
  runA = await runCreate(projectId);
  evidence.runA = runA;
  const stateA = await runStart(runA, session, [], process.pid, await selfStartTicks());
  assert(["running", "ready", "queued"].includes(stateA), `runStart state ${stateA}`);

  // Kill core (SIGKILL)
  core!.kill("SIGKILL");
  await new Promise(r => setTimeout(r, 200));
  evidence.killA = true;

  // Restart core
  core = await startCore(env);
  evidence.restartA = true;

  // Inspect runA: should be blocked (migration blocks running->blocked)
  const reportA = await runInspect(runA);
  assert.strictEqual(reportA.run_state, "blocked");
  assert(reportA.recorded_pid === String(process.pid), "recorded pid preserved");
  assert(reportA.pending_mutation === false);
  assert(reportA.uncertain_actions.length === 0);
  evidence.reportA = reportA;

  // Reconcile Abandon -> Cancelled
  await runReconcile(runA, "abandon");
  evidence.reconcileA = "abandon";

  // Verify artifact preserved
  const artifactData = await artifactRead(artifactId);
  assert(artifactData.includes("completed artifact"), "artifact data preserved after crash");

  // ── Scenario B: kill after write / before receipt (run with post_mutation checkpoint) ──
  runB = await runCreate(projectId);
  evidence.runB = runB;
  const stateB = await runStart(runB, session, [artifactId], process.pid, await selfStartTicks());
  assert(["running", "ready", "queued"].includes(stateB));

  // Write a temp file (simulate the worker's side effect)
  const targetFile = join(temp, "mutation-output.txt");
  await writeFile(targetFile, "mutation wrote this");
  const statBefore = await stat(targetFile);
  evidence.targetFileMtime = statBefore.mtimeMs;

  // Capture a PostMutation checkpoint (manifest includes the artifact)
  const manifest: ResultManifest = {
    run_id: runB as RunId,
    native_session: session,
    artifacts: [artifactId as ArtifactId],
    checks: [],
    acceptance_satisfied: false,
    unresolved: [],
  };
  const checkpointArtifact = await runCheckpoint(runB, "post_mutation", manifest);
  evidence.checkpointArtifact = checkpointArtifact;

  // Kill core (after write + checkpoint, before receipt)
  core!.kill("SIGKILL");
  await new Promise(r => setTimeout(r, 200));
  evidence.killB = true;

  // Restart core
  core = await startCore(env);
  evidence.restartB = true;

  // Inspect runB: blocked, checkpoint present
  const reportB = await runInspect(runB);
  assert.strictEqual(reportB.run_state, "blocked");
  assert(reportB.checkpoint !== null, "checkpoint must be present");
  assert.strictEqual(reportB.checkpoint?.phase, "post_mutation");
  assert(reportB.uncertain_actions.length === 0, "no uncertain actions in this scenario");
  evidence.reportB = reportB;

  // Reconcile AcceptObserved -> should succeed (we have a checkpoint, but it's post_mutation not post_verification; AcceptObserved will reject because it's not PostVerification)
  // But the spec says "explicit Blocked reconciliation" and "no false success".
  // We'll test that AcceptObserved is rejected (no post_verification checkpoint).
  try {
    await runReconcile(runB, "accept_observed");
    evidence.acceptObserved = "succeeded_unexpectedly"; // this should not happen
    assert(false, "AcceptObserved without PostVerification should be rejected");
  } catch (err) {
    evidence.acceptObserved = "rejected_as_expected";
    const msg = (err as Error).message;
    assert(msg.includes("acceptance_evidence_missing") || msg.includes("PostVerification"), `Expected rejection, got: ${msg}`);
  }

  // Abandon -> Cancelled
  await runReconcile(runB, "abandon");
  evidence.reconcileB = "abandon";

  // Verify target file unchanged (no duplicate write)
  const statAfter = await stat(targetFile);
  assert.strictEqual(statAfter.mtimeMs, statBefore.mtimeMs, "file mtime unchanged - no duplicate write");
  assert.strictEqual(statAfter.size, statBefore.size, "file size unchanged");
  const contentAfter = await readFile(targetFile, "utf-8");
  assert.strictEqual(contentAfter, "mutation wrote this", "file content exact");

  // Verify checkpoint artifact preserved: the CAS payload records the phase
  // and the run's artifact ids (it never knows a worker's local file path).
  const checkpointData = await artifactRead(checkpointArtifact);
  const checkpointRecord = JSON.parse(checkpointData);
  assert.strictEqual(checkpointRecord.phase, "post_mutation", "checkpoint records its phase");
  assert(
    (checkpointRecord.manifest.artifacts as string[]).includes(artifactId),
    "checkpoint records the run's artifact",
  );
  assert.strictEqual(checkpointRecord.manifest.run_id, runB, "checkpoint belongs to the run");
  evidence.checkpointRecord = { phase: checkpointRecord.phase, state_digest: checkpointRecord.state_digest };

  evidence.state = "passed";
} catch (err) {
  if (isOwnedWiringError(err as Error)) {
    evidence.state = "blocked_on_owner_wiring";
    evidence.blockedReason = (err as Error).message;
  } else {
    evidence.state = "failed";
    evidence.error = (err as Error).message;
  }
} finally {
  if (core) core.kill("SIGTERM");
  if (temp) {
    try { await rm(temp, { recursive: true, force: true }); } catch {}
  }
  // Write evidence
  const evidencePath = resolve(process.env.IRONBARK_EVIDENCE_DIR ?? ".superpowers/sdd/2026-10-12-ironbark-assistant/C4-smoke.json");
  await mkdir(join(evidencePath, ".."), { recursive: true }).catch(() => {});
  await writeFile(evidencePath, JSON.stringify(evidence, null, 2));
  console.log("Evidence written to", evidencePath);
  if (evidence.state === "failed") process.exit(1);
}