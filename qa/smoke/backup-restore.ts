import { strict as assert } from "node:assert";
import { createHash } from "node:crypto";
import { cp, mkdtemp, mkdir, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { Subprocess } from "bun";
import { Database } from "bun:sqlite";
import type { BackupManifest, ImportPreview, ImportReceipt, RecoveryReport, RestoreReport, RunReceipt } from "../../packages/contracts/src/generated";

// Exercises actual future canonical CLI operations. Unsupported commands fail;
// this script never synthesizes a database, a receipt, or a successful response.
const daemonBinary = resolve(process.env.IRONBARK_CORE_BIN ?? "target/debug/ironbark-core");
const cliBinary = resolve(process.env.IRONBARK_CLI_BIN ?? "target/debug/ironbark-cli");
const temp = await mkdtemp(join(tmpdir(), "ibc5-"));
const secret = `C5_UNIQUE_SECRET_${crypto.randomUUID()}`;
interface SmokeEnvironment extends NodeJS.ProcessEnv {
  HOME: string;
  XDG_DATA_HOME: string;
  XDG_STATE_HOME: string;
  XDG_RUNTIME_DIR: string;
}
function environment(name: string): SmokeEnvironment {
  const root = join(temp, name);
  return { ...process.env, HOME: root, XDG_DATA_HOME: join(root, "d"), XDG_STATE_HOME: join(root, "s"), XDG_RUNTIME_DIR: join(root, "r") };
}
const originalEnv = environment("original");
const restoredEnv = environment("restored");
const data = (env: SmokeEnvironment) => join(env.XDG_DATA_HOME, "ironbark-assistant");
const runtime = (env: SmokeEnvironment) => join(env.XDG_RUNTIME_DIR, "ironbark-assistant");
let daemon: Subprocess<"ignore", "pipe", "pipe"> | undefined;
let restoredDaemon: Subprocess<"ignore", "pipe", "pipe"> | undefined;
function deadline<T>(promise: Promise<T>, ms = 30_000): Promise<T> {
  const expired = Promise.withResolvers<never>();
  const timer = setTimeout(() => expired.reject(new Error("C5 real-operation timeout")), ms);
  return Promise.race([promise, expired.promise]).finally(() => clearTimeout(timer));
}
async function exists(path: string): Promise<boolean> {
  try { await stat(path); return true; } catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return false; throw error; }
}
async function command(env: SmokeEnvironment, args: string[], expectedCode = 0): Promise<Buffer> {
  const child = Bun.spawn([cliBinary, ...args], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  try {
    const [stdout, stderr, code] = await deadline(Promise.all([new Response(child.stdout).arrayBuffer(), new Response(child.stderr).text(), child.exited]));
    if (expectedCode === 0) assert.equal(code, 0, `${args.join(" ")}: ${stderr}`);
    else { assert.notEqual(code, 0, "negative restore unexpectedly succeeded"); assert.match(stderr + Buffer.from(stdout).toString(), new RegExp(expectedCode === 1 ? "IntegrityMismatch" : expectedCode === 2 ? "NamespaceExists" : "UnknownBackupSchema")); }
    return Buffer.from(stdout);
  } finally { if (child.exitCode === null) { child.kill("SIGKILL"); await child.exited; } }
}
async function json<T>(env: SmokeEnvironment, args: string[]): Promise<T> { return JSON.parse((await command(env, args)).toString("utf8")) as T; }
async function start(env: SmokeEnvironment): Promise<Subprocess<"ignore", "pipe", "pipe">> {
  for (const path of [env.XDG_DATA_HOME, env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR]) await mkdir(path, { recursive: true, mode: 0o700 });
  const child = Bun.spawn([daemonBinary], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  try {
    const reader = child.stdout.getReader();
    await deadline((async () => { let text = ""; while (true) { const result = await reader.read(); if (result.done) throw new Error("real daemon exited before readiness"); text += new TextDecoder().decode(result.value); if (text.includes("CORE_READY protocol=1")) { reader.releaseLock(); return; } } })());
    return child;
  } catch (error) { child.kill("SIGKILL"); await child.exited; throw error; }
}
async function stop(env: SmokeEnvironment, child: Subprocess): Promise<void> {
  await command(env, ["shutdown"]); assert.equal(await deadline(child.exited), 0);
}
async function allFiles(path: string): Promise<string[]> {
  const result: string[] = [];
  for (const entry of await readdir(path, { withFileTypes: true })) {
    const file = join(path, entry.name); assert(!entry.isSymbolicLink(), "backup contains symlink");
    if (entry.isDirectory()) result.push(...await allFiles(file)); else result.push(file);
  }
  return result;
}
const sha256 = (bytes: Buffer) => createHash("sha256").update(bytes).digest("hex");
async function assertNoChildren(pid: number): Promise<void> {
  const live: number[] = [];
  for (const entry of await readdir("/proc")) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const text = await readFile(`/proc/${entry}/stat`, "utf8");
      const suffix = text.slice(text.lastIndexOf(")") + 2).split(" ");
      if (Number(suffix[1]) === pid) live.push(Number(entry));
    } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  }
  assert.deepEqual(live, [], "restored daemon owns live worker children");
}
try {
  daemon = await start(originalEnv);
  const project = await json<{ id: string }>(originalEnv, ["project", "create", "backup-smoke", "--root", temp]);
  const sourcePath = join(temp, "source.txt"); const sourceBytes = Buffer.from("immutable old citation revision\n");
  await writeFile(sourcePath, sourceBytes);
  const sourceDocument = join(temp, "source-import.json"); await writeFile(sourceDocument, JSON.stringify({ kind: "source", uri: "file:source.txt", path: "source.txt" }));
  const preview = await json<ImportPreview>(originalEnv, ["import", "preview", "--project", project.id, "--file", sourceDocument]);
  const imported = await json<ImportReceipt>(originalEnv, ["import", "apply", "--project", project.id, "--digest", preview.digest]);
  assert(imported.artifact_id && imported.source_id);
  assert.deepEqual(await command(originalEnv, ["artifact", "read", "--project", project.id, "--id", imported.artifact_id]), sourceBytes);
  // This is a real queued scheduler run, not a fake native completion fixture.
  const run = await json<RunReceipt>(originalEnv, ["run", "create", "--project", project.id, "--task", "queued backup smoke work", "--workspace", temp]);
  const checkpointFile = join(temp, "checkpoint-manifest.json");
  await writeFile(checkpointFile, JSON.stringify({ run_id: run.run_id, artifacts: [imported.artifact_id], checks: [], acceptance_satisfied: false, unresolved: ["queued work; not executed"] }));
  const checkpointArtifact = await json<string>(originalEnv, ["checkpoint", "capture", "--project", project.id, "--run", run.run_id, "--phase", "planned", "--manifest", checkpointFile]);
  const checkpointBytes = await command(originalEnv, ["artifact", "read", "--project", project.id, "--id", checkpointArtifact]);
  // Unique runtime/vault sentinel is actual file material that must not export.
  await writeFile(join(runtime(originalEnv), "private-lease-sentinel"), secret, { mode: 0o600 });
  const native = join(temp, "native-harness"); await mkdir(native, { mode: 0o700 }); await writeFile(join(native, "credentials"), secret, { mode: 0o600 });
  const backup = join(temp, "backup");
  const manifest = await json<BackupManifest>(originalEnv, ["backup", "create", "--destination", backup]);
  assert.equal(manifest.format, 1);
  assert.equal(sha256(await readFile(join(backup, "core.sqlite3"))), manifest.database_digest);
  assert(manifest.objects.some((o) => o.digest === sha256(sourceBytes)));
  assert(manifest.objects.some((o) => o.digest === sha256(checkpointBytes)));
  for (const object of manifest.objects) { const bytes = await readFile(join(backup, "objects", object.digest)); assert.equal(bytes.length, object.bytes); assert.equal(sha256(bytes), object.digest); }
  for (const path of await allFiles(backup)) assert(!(await readFile(path)).includes(Buffer.from(secret)), "unique secret sentinel leaked into export");
  // Core remains live throughout online backup.
  const liveStatus = await json<{ projects: number }>(originalEnv, ["status"]); assert.equal(liveStatus.projects, 1);
  const report = await json<RestoreReport>(restoredEnv, ["backup", "restore", "--source", backup, "--new-namespace"]);
  assert.equal(report.stopped, true); assert(BigInt(report.blocked_runs) >= 1n);
  assert(!(await exists(runtime(restoredEnv)))); assert(!(await exists(join(restoredEnv.XDG_STATE_HOME, "ironbark-assistant"))));
  restoredDaemon = await start(restoredEnv);
  assert.deepEqual(await command(restoredEnv, ["artifact", "read", "--project", project.id, "--id", imported.artifact_id]), sourceBytes);
  assert.deepEqual(await command(restoredEnv, ["artifact", "read", "--project", project.id, "--id", checkpointArtifact]), checkpointBytes);
  const restoredRun = await json<RecoveryReport>(restoredEnv, ["run", "inspect", "--project", project.id, "--run", run.run_id]); assert.equal(restoredRun.run_state, "blocked");
  const processStatus = await json<{ protocol: number; projects: number }>(restoredEnv, ["status"]);
  assert.equal(processStatus.protocol, 1, "restored core is not the canonical protocol");
  assert.equal(processStatus.projects, 1, "restored namespace lost its restored project");
  await assertNoChildren(restoredDaemon.pid);
  assert.equal((await readFile(join(native, "credentials"), "utf8")), secret);
  const doctor = await json<{ sqlite_integrity: string; foreign_keys: boolean }>(restoredEnv, ["doctor", "--json"]); assert.equal(doctor.sqlite_integrity, "ok"); assert.equal(doctor.foreign_keys, true);
  await stop(restoredEnv, restoredDaemon); restoredDaemon = undefined;
  // Restore never overwrites an existing namespace, even after its core stops.
  const before = sha256(await readFile(join(data(restoredEnv), "core.sqlite3")));
  await command(restoredEnv, ["backup", "restore", "--source", backup, "--new-namespace"], 2);
  assert.equal(sha256(await readFile(join(data(restoredEnv), "core.sqlite3"))), before);
  for (const mode of ["missing", "corrupt"] as const) {
    const broken = join(temp, `backup-${mode}`); await cp(backup, broken, { recursive: true });
    const object = join(broken, "objects", manifest.objects[0].digest);
    if (mode === "missing") await rm(object); else await writeFile(object, Buffer.from("corrupted artifact"));
    const env = environment(`rejected-${mode}`); await command(env, ["backup", "restore", "--source", broken, "--new-namespace"], 1);
    assert(!(await exists(data(env)))); assert(!(await exists(runtime(env))));
  }
  const unknownBackup = join(temp, "backup-unknown-schema");
  await cp(backup, unknownBackup, { recursive: true });
  const unknownDatabase = join(unknownBackup, "core.sqlite3");
  const unreviewed = new Database(unknownDatabase);
  try { unreviewed.exec("CREATE TABLE future_object_references(id TEXT PRIMARY KEY, body TEXT NOT NULL)"); }
  finally { unreviewed.close(); }
  const unknownBytes = await readFile(unknownDatabase);
  const unknownManifest = { ...manifest, database_digest: sha256(unknownBytes), database_bytes: unknownBytes.length };
  await writeFile(join(unknownBackup, "manifest.json"), JSON.stringify(unknownManifest));
  const unknownEnv = environment("rejected-schema");
  await command(unknownEnv, ["backup", "restore", "--source", unknownBackup, "--new-namespace"], 3);
  assert(!(await exists(data(unknownEnv))));
  assert(!(await exists(runtime(unknownEnv))));
  assert.equal(sha256(await readFile(unknownDatabase)), unknownManifest.database_digest);
  await stop(originalEnv, daemon); daemon = undefined;
  console.log(JSON.stringify({ source_hash: sha256(sourceBytes), checkpoint_hash: sha256(checkpointBytes), manifest_objects: manifest.objects.length, restored_runs: "blocked", secret_sentinel: "absent", native_target: "untouched", corrupt_and_missing: "rejected before install", unknown_schema: "rejected before install with input unchanged", sqlite_integrity: doctor.sqlite_integrity, native_workers: "none observed" }));
} finally {
  for (const child of [daemon, restoredDaemon]) if (child && child.exitCode === null) { child.kill("SIGINT"); try { await deadline(child.exited, 5000); } catch { child.kill("SIGKILL"); await child.exited; } }
  await rm(temp, { recursive: true, force: true });
}
