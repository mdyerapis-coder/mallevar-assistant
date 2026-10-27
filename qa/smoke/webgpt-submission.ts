//! W1-W3 named-blocker smoke: prove the real prebuilt core + CLI fails closed
//! and honestly when no ChatGPT account exists. It never logs in, never uses a
//! canned model response, and never requires the network.
import { strict as assert } from "node:assert";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Database } from "bun:sqlite";
import type { Subprocess } from "bun";
import type { ConnectionConfig, ContentPart, CoreCommand, Project, WebGptStatus } from "../../packages/contracts/src/generated";

const daemonBinary = resolve(process.env.IRONBARK_CORE_BIN ?? "target/debug/ironbark-core");
const cliBinary = resolve(process.env.IRONBARK_CLI_BIN ?? "target/debug/ironbark-cli");
const evidence: Record<string, unknown> & { state: string } = { state: "running", mechanism: "actual prebuilt core + CLI, no sidecar installed, no account", account: false, simulated_model_success: false };
const temp = await mkdtemp(join(tmpdir(), "ibwebgpt-"));
const env = { ...process.env, HOME: temp, XDG_DATA_HOME: join(temp, "d"), XDG_STATE_HOME: join(temp, "s"), XDG_RUNTIME_DIR: join(temp, "r") };
for (const dir of [env.XDG_DATA_HOME, env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR]) await mkdir(dir, { mode: 0o700 });
const dataDir = join(env.XDG_DATA_HOME, "ironbark-assistant");

/** Decode byte chunks until `predicate` matches an accumulated line. */
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

let daemon: Subprocess<"ignore", "pipe", "pipe"> | undefined;
async function start(): Promise<void> {
  const child = Bun.spawn([daemonBinary], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  daemon = child;
  const timer = setTimeout(() => child.kill("SIGTERM"), 15_000);
  try {
    if (await readUntilReady(child.stdout, (line) => line.includes("CORE_READY protocol=1"))) return;
    const stderr = await new Response(child.stderr).text();
    throw new Error(`core did not report ready: ${stderr.trim()}`);
  } finally { clearTimeout(timer); }
}
async function stop(): Promise<void> {
  if (!daemon) return;
  const child = daemon;
  daemon = undefined;
  child.kill("SIGINT");
  const timer = setTimeout(() => child.kill("SIGKILL"), 5_000);
  try { await child.exited; } finally { clearTimeout(timer); }
}

interface CliResult { status: number; out: string; err: string }
async function cli(command: CoreCommand): Promise<CliResult> {
  const child = Bun.spawn([cliBinary, "command", "--request-id", crypto.randomUUID(), JSON.stringify(command)], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGTERM"), 60_000);
  try {
    const [out, err, status] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
    return { status, out, err };
  } finally { clearTimeout(timer); }
}
async function cliOk<T>(command: CoreCommand): Promise<T> {
  const result = await cli(command);
  assert.equal(result.status, 0, `CLI failure (${result.status}): ${result.err.slice(0, 512)}`);
  // The CLI prints the typed leaf value, not the CoreReply envelope
  // (crates/ironbark-cli emit(): leaf_json); a core failure exits non-zero.
  return JSON.parse(result.out) as T;
}
async function cliError(command: CoreCommand): Promise<{ code: string; message: string; status: number }> {
  const result = await cli(command);
  assert.notEqual(result.status, 0, "CLI must exit nonzero on a core error");
  const match = /^([a-z0-9_]+): ([\s\S]*)$/.exec(result.err.trim());
  assert(match, `stderr must carry the typed code, got: ${result.err.slice(0, 256)}`);
  return { code: match[1], message: match[2], status: result.status };
}

let project = "";
let run = "";
try {
  await start();
  const created = await cliOk<Project>({ type: "project_create", name: "webgpt-smoke", root: join(temp, "root") });
  project = created.id;
  const bridge: ConnectionConfig = { id: "webgpt", protocol: "custom", endpoint: "bridge://webgpt", models: [], allowed_projects: [project] };
  await stop();
  // Seed the run + the bridge connection into the live store while the core is stopped.
  {
    const db = new Database(join(dataDir, "core.sqlite3"));
    db.query("INSERT INTO provider_references (id, config) VALUES ('webgpt', ?1)").run(JSON.stringify(bridge));
    const job = crypto.randomUUID();
    const node = crypto.randomUUID();
    run = crypto.randomUUID();
    db.query("INSERT INTO jobs (id, project_id, body, state) VALUES (?1, ?2, '{}', 'queued')").run(job, project);
    db.query("INSERT INTO nodes (id, job_id, body, state) VALUES (?1, ?2, '{}', 'queued')").run(node, job);
    db.query("INSERT INTO runs (id, node_id, state, body) VALUES (?1, ?2, 'queued', '{}')").run(run, node);
    db.close();
  }
  await start();

  // 1. probe: an honest non-activated state (no sidecar, no account), never activated.
  const status = await cliOk<WebGptStatus>({ type: "web_gpt_probe", connection: bridge });
  assert(["not_installed", "not_activated", "unavailable"].includes(status.state), `probe must report a non-activated state, got ${status.state}`);
  assert.notEqual(status.state, "activated");
  assert(status.reason, "probe must carry the exact reason");

  // 2. activate: always owner-interactive, never automated.
  const activate = await cliError({ type: "web_gpt_activate", connection_id: "webgpt" });
  assert.equal(activate.code, "interactive_required");

  // 3. submit: fails closed with the typed not-activated error; no fake success.
  const key = crypto.randomUUID();
  const content: ContentPart[] = [{ type: "text", text: "harmless offline smoke; no sidecar installed" }];
  const first = await cliError({ type: "web_gpt_submit", run_id: run, content, idempotency_key: key });
  assert(["webgpt_not_activated", "webgpt_connection_missing"].includes(first.code), `submit must fail closed, got ${first.code}: ${first.message}`);

  // A duplicate submit with the same key: the same typed failure, never a second send.
  const second = await cliError({ type: "web_gpt_submit", run_id: run, content, idempotency_key: key });
  assert.deepEqual([second.code, second.message], [first.code, first.message]);

  // 4. resubmit without a receipt: typed not-found.
  const resubmit = await cliError({ type: "web_gpt_resubmit", run_id: run, receipt_digest: "0".repeat(64) });
  assert.equal(resubmit.code, "receipt_not_found");

  evidence.state = "passed";
  Object.assign(evidence, { project, run, probe_state: status.state, activate_code: activate.code, submit_code: first.code, duplicate_submit: "identical typed failure", resubmit_code: resubmit.code });
} catch (error) {
  evidence.state = "blocked_or_failed";
  evidence.error = error instanceof Error ? error.message : String(error);
  process.exitCode = 1;
} finally {
  await stop().catch(() => {});
  await rm(temp, { recursive: true, force: true });
  const reportDir = ".superpowers/sdd/2026-10-12-ironbark-assistant";
  await mkdir(reportDir, { recursive: true });
  const report = join(reportDir, "W1-W3-smoke.json");
  await Bun.write(report, JSON.stringify(evidence, null, 2) + "\n");
  console.log(JSON.stringify(evidence));
}
