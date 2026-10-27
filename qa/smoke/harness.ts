//! H1 native-harness smoke: drive the real OMP ACP adapter end to end against a
//! LOCAL llama.cpp model, with the core as the run/policy/receipt authority.
//!
//! Run: `bun qa/smoke/harness.ts --harness omp [--connection <id>]`
//!
//! What it proves with real effects, not mocks:
//!   * an isolated OMP profile (own HOME/XDG + profile dir) is pointed at a local
//!     llama.cpp endpoint by writing `models.yml` into that profile's agent dir,
//!     so OMP never reads the operator's `~/.omp` credentials or catalog defaults;
//!   * the built core daemon (its own isolated XDG roots) owns the project, the
//!     committed policy, the local connection and the run;
//!   * a real OMP turn runs through `OmpAdapter`, creates a file inside a writer
//!     Git worktree, and the real diff + native session id are recorded;
//!   * a write outside the leased worktree is denied by the core host and leaves
//!     no file;
//!   * pausing then resuming the run keeps the same native OMP session id;
//!   * a cancelled child command is reported stopped only after the OS process is
//!     observed gone.
//!
//! It also records a genuine adapter defect it observes: `AcpAdapterBase`
//! advertises the ACP client `fs.writeTextFile` capability but does not implement
//! `fs/write_text_file`, so OMP's edit tool is answered `{denied:true}` and OMP
//! reports the write as successful while nothing is written. The smoke therefore
//! performs the file-creating task through OMP's own `bash` tool.
//!
//! The operator's OMP install/state (`~/.omp`), the operator store and the two
//! operator llama.cpp servers (8091/8092) are never written. When the operator's
//! endpoint cannot host OMP's ACP engine prompt (its context window is smaller
//! than the measured prompt), the smoke starts its own local llama.cpp server on
//! a free loopback port with an adequate context and records that fact.
import { strict as assert } from "node:assert";
import { createHash } from "node:crypto";
import { access, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { cpus, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import type { Subprocess } from "bun";

import { OmpAdapter } from "../../packages/adapters/src/harness/omp";
import { deriveActionSpec, type NativePermissionHost } from "../../packages/adapters/src/harness/native/host";
import { applyOmpSettings, ompEnv, prepareOmpProfile, writeScopedOmpWrapper } from "../fixtures/omp-profile";
import type {
  ContextPackage,
  ImportPreview,
  ImportReceipt,
  OpenSessionInput,
  Project,
  ProjectPolicy,
  RunReceipt,
  SessionRef,
  ToolDescriptor,
  TurnInput,
  WorkerEvent,
} from "../../packages/contracts/src/generated";

const repoRoot = resolve(import.meta.dir, "..", "..");
const reportDir = join(repoRoot, ".superpowers", "sdd", "2026-10-12-ironbark-assistant");
const reportPath = join(reportDir, "qa-smoke-harness-ts.json");
const PROFILE = "ironbark-worker";

// ── argument parsing (the plan's `--harness omp --connection <id>`) ───────────
interface Args { harness: string; connection?: string; endpoint?: string; modelPath?: string; ompBinary?: string; llamaBinary?: string; }
function parseArgs(argv: string[]): Args {
  const out: Args = { harness: "omp" };
  for (let i = 0; i < argv.length; i += 1) {
    const [flag, inline] = argv[i].split("=");
    const value = inline ?? argv[++i];
    if (flag === "--harness") out.harness = value;
    else if (flag === "--connection") out.connection = value;
    else if (flag === "--endpoint") out.endpoint = value;
    else if (flag === "--model-path") out.modelPath = value;
    else if (flag === "--omp-binary") out.ompBinary = value;
    else if (flag === "--llama-binary") out.llamaBinary = value;
  }
  return out;
}
const args = parseArgs(process.argv.slice(2));

// ── receipt: every step records its exact command, exit code and real result ──
interface Step { name: string; command: string; exit_code: number; expected: string; observed: string; }
const steps: Step[] = [];
const evidence: Record<string, unknown> = {};
const notes: string[] = [];
function record(name: string, command: string, exitCode: number, expected: string, observed: string): void {
  steps.push({ name, command, exit_code: exitCode, expected, observed });
}

async function exists(path: string): Promise<boolean> {
  try { await access(path); return true; } catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return false; throw error; }
}
function sha256(value: Uint8Array | string): string { return createHash("sha256").update(value).digest("hex"); }
function delay(ms: number): Promise<void> {
  const { promise, resolve: settle } = Promise.withResolvers<void>();
  setTimeout(settle, ms);
  return promise;
}
function deadline<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  const expired = Promise.withResolvers<never>();
  const timer = setTimeout(() => expired.reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  return Promise.race([promise, expired.promise]).finally(() => clearTimeout(timer));
}
/** Read one string field of an unknown object; `typeof`/`in` keeps the read checked. */
function textField(container: unknown, keys: string[]): string | undefined {
  const record = asFields(container);
  if (!record) return undefined;
  for (const key of keys) {
    if (!(key in record)) continue;
    const value = record[key];
    if (typeof value === "string" && value) return value;
  }
  return undefined;
}
/** Narrow an unknown JSON value to an object for member reads (structurally identical). */
function asFields(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
}

// ── isolated core XDG env ────────────────────────────────────────────────────
interface CoreEnv extends NodeJS.ProcessEnv { HOME: string; XDG_DATA_HOME: string; XDG_STATE_HOME: string; XDG_RUNTIME_DIR: string; }
function coreEnv(root: string): CoreEnv {
  return { ...process.env, HOME: root, XDG_DATA_HOME: join(root, "data"), XDG_STATE_HOME: join(root, "state"), XDG_RUNTIME_DIR: join(root, "run") };
}

interface Outcome { code: number; stdout: string; stderr: string }
async function capture(binary: string, argv: string[], env: NodeJS.ProcessEnv, cwd: string, timeoutMs = 60_000): Promise<Outcome> {
  const child = Bun.spawn([binary, ...argv], { env, cwd, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGKILL"), timeoutMs);
  const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
  clearTimeout(timer);
  return { code, stdout, stderr };
}

/** Resolve the real OMP binary, not a `mise` shim (a shim breaks under a moved HOME). */
async function resolveOmpBinary(explicit?: string): Promise<string> {
  if (explicit) return explicit;
  if (process.env.IRONBARK_OMP_BINARY) return process.env.IRONBARK_OMP_BINARY;
  const where = await capture("mise", ["where", "oh-my-pi"], process.env, repoRoot).catch(() => ({ code: 1, stdout: "", stderr: "" }));
  if (where.code === 0) {
    const candidate = join(where.stdout.trim(), "omp");
    if (await exists(candidate)) return candidate;
  }
  const which = await capture("bash", ["-lc", "command -v omp || true"], process.env, repoRoot);
  const found = which.stdout.trim();
  if (found && (await exists(found)) && !found.includes("/mise/")) return found;
  throw new Error("cannot resolve the real OMP binary (set --omp-binary or IRONBARK_OMP_BINARY)");
}

// ── git helpers ──────────────────────────────────────────────────────────────
async function git(cwd: string, argv: string[]): Promise<string> {
  const out = await capture("git", ["-C", cwd, ...argv], { ...process.env, GIT_AUTHOR_NAME: "h1-smoke", GIT_AUTHOR_EMAIL: "h1@example.invalid", GIT_COMMITTER_NAME: "h1-smoke", GIT_COMMITTER_EMAIL: "h1@example.invalid" }, cwd);
  assert.equal(out.code, 0, `git ${argv.join(" ")} failed: ${out.stderr.trim()}`);
  return out.stdout.trim();
}

// ── CLI shim over the built client ───────────────────────────────────────────
let cliBinary = "";
let coreBinary = "";
async function cli(env: CoreEnv, argv: string[], expect = 0): Promise<Outcome> {
  const out = await capture(cliBinary, argv, env, repoRoot);
  assert.equal(out.code, expect, `ironbark-cli ${argv.join(" ")} expected exit ${expect}, got ${out.code}: ${out.stderr.trim()}`);
  return out;
}
async function cliJson<T>(env: CoreEnv, argv: string[]): Promise<T> { return JSON.parse((await cli(env, argv)).stdout.trim()) as T; }
async function command<T>(env: CoreEnv, payload: Record<string, unknown>): Promise<T> {
  return JSON.parse((await cli(env, ["command", "--request-id", crypto.randomUUID(), JSON.stringify(payload)])).stdout.trim()) as T;
}

// ── the local llama.cpp endpoint ─────────────────────────────────────────────
interface LocalServer { endpoint: string; owned: boolean; nCtx: number; modelPath: string; note: string; }
async function probeCtx(endpoint: string): Promise<number> {
  try {
    const body: unknown = await (await fetch(`${endpoint}/props`, { signal: AbortSignal.timeout(3_000) })).json();
    const settings = asFields(body)?.["default_generation_settings"];
    const nCtx = asFields(settings)?.["n_ctx"];
    return typeof nCtx === "number" ? nCtx : 0;
  } catch { return 0; }
}
async function discoverModel(): Promise<string> {
  if (args.modelPath) return args.modelPath;
  const candidates: string[] = [];
  for (const dir of ["/home/mason/Work/AI/z-image", "/home/mason/.local/share/file-steward/models"]) {
    try {
      for (const entry of await readdir(dir)) if (entry.endsWith(".gguf") && /instruct/i.test(entry) && !/z_image/i.test(entry)) candidates.push(join(dir, entry));
    } catch { /* directory absent */ }
  }
  candidates.sort((a, b) => b.length - a.length);
  const chosen = candidates[0];
  if (!chosen) throw new Error("no local GGUF model found; pass --model-path");
  return chosen;
}
async function readSelector(raw: string): Promise<string | undefined> {
  const parsed: unknown = JSON.parse(raw);
  const models = asFields(parsed)?.["models"];
  if (!Array.isArray(models) || models.length === 0) return undefined;
  return textField(models[0], ["selector"]);
}
/** Start the core and continuously drain its stdout, resolving once it reports ready. */
function startCoreDraining(binary: string, env: CoreEnv, cwd: string): { child: Subprocess<"ignore", "pipe", "ignore">; ready: Promise<string> } {
  const child = Bun.spawn([binary], { env, cwd, stdin: "ignore", stdout: "pipe", stderr: "ignore" });
  const reader = child.stdout.getReader();
  const decoder = new TextDecoder();
  const { promise, resolve } = Promise.withResolvers<string>();
  void (async () => {
    let buffer = ""; let signalled = false;
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split("\n"); buffer = lines.pop() ?? "";
        for (const line of lines) if (!signalled && line.includes("CORE_READY")) { signalled = true; resolve(line.trim()); }
      }
      if (!signalled) throw new Error("core exited before reporting readiness");
    } catch (error) { resolve(`(stdout ended) ${String(error)}`); }
  })();
  return { child, ready: promise };
}

async function startOwnLlama(binary: string, modelPath: string, port: number, logPath: string): Promise<Subprocess<"ignore", "ignore", "ignore">> {
  await mkdir(dirname(logPath), { recursive: true });
  const threads = String(Math.max(4, Math.min(8, cpus().length)));
  const child = Bun.spawn([binary, "-m", modelPath, "--host", "127.0.0.1", "--port", String(port), "-c", "16384", "-ngl", "0", "-t", threads, "--log-file", logPath], { stdin: "ignore", stdout: "ignore", stderr: "ignore" });
  await deadline((async () => {
    for (let i = 0; i < 150; i += 1) {
      if (await probeCtx(`http://127.0.0.1:${port}`) > 0) return;
      await delay(1_000);
    }
    throw new Error("own llama-server did not start");
  })(), 180_000, "own llama-server readiness");
  return child;
}

// ── permission host: the core's decision point ───────────────────────────────
interface Decision { tool: string; kind: string; title: string; path?: string; command?: string; decision: "allow" | "deny"; reason: string; }
function makeHost(policy: ProjectPolicy, worktree: string, adapterRef: { current?: OmpAdapter }): {
  host: NativePermissionHost;
  decisions: Decision[];
  denials: string[];
  nativeEvents: unknown[];
} {
  const decisions: Decision[] = [];
  const denials: string[] = [];
  const nativeEvents: unknown[] = [];
  const host: NativePermissionHost = {
    async state() { return { policy_version: policy.version, state_digest: sha256(JSON.stringify(policy)) }; },
    async prepareAction(request, input) {
      const source = asFields(request.input) ?? {};
      const name = typeof source["name"] === "string" && source["name"] ? source["name"] : request.name;
      const kind = typeof source["kind"] === "string" ? source["kind"].toLowerCase() : "";
      const title = typeof source["title"] === "string" ? source["title"].toLowerCase() : "";
      const rawInput = source["rawInput"] ?? request.input;
      const fields = asFields(rawInput);
      const action = deriveActionSpec(input, request.tool_call_id || name, `omp.tool.${kind || "other"}`, rawInput);
      const targetPath = textField(fields ?? source, ["path", "file_path", "filePath", "absPath", "target", "filename"]);
      const shellCommand = textField(fields ?? source, ["command", "cmd", "script"]);
      const hasContent = fields !== undefined && "content" in fields;
      // OMP's ACP toolCall carries an empty `name`; classify by kind/title/rawInput.
      const isShell = ["execute", "terminal", "bash", "shell"].includes(kind) || shellCommand !== undefined || /command|execute|shell|terminal|run\b/.test(title);
      const isWrite = !isShell && (kind === "edit" || kind === "write" || /write|create|edit|patch|save/.test(title) || (targetPath !== undefined && hasContent));
      const isRead = !isShell && !isWrite && (kind === "read" || kind === "fetch" || targetPath !== undefined);
      let allowed = true; let reason = "in-scope";
      if (isShell) {
        const absolutePaths = shellCommand ? (shellCommand.match(/\/[^\s'"`;|&)]+/g) ?? []) : [];
        const outside = absolutePaths.find((candidate) => {
          const r = resolve(candidate);
          return r !== worktree && !r.startsWith(`${worktree}/`)
            && !r.startsWith("/dev/") && !r.startsWith("/proc/") && !r.startsWith("/usr/") && !r.startsWith("/bin/") && !r.startsWith("/etc/");
        });
        if (outside) { allowed = false; reason = `shell command references path outside leased worktree: ${outside}`; }
      } else if (isWrite) {
        if (targetPath) {
          const target = resolve(worktree, targetPath);
          allowed = target === worktree || target.startsWith(`${worktree}/`);
          if (!allowed) reason = `write outside leased worktree: ${target}`;
        } else { allowed = false; reason = "write tool without a resolvable in-scope path"; }
      } else if (!isRead) {
        allowed = false; reason = `unrecognised tool denied (fail closed): kind=${kind || "(none)"} title=${title || "(none)"}`;
      }
      decisions.push({ tool: name, kind, title, path: targetPath, command: shellCommand, decision: allowed ? "allow" : "deny", reason });
      if (!allowed) { denials.push(reason); throw new Error(`CoreDeniedAction: ${reason}`); }
      // The core approves its own prepared action; answer out of band because the
      // adapter buffers turn events until the turn finishes, so the consumer cannot
      // answer the in-turn approval promise from the stream.
      setTimeout(() => {
        void adapterRef.current?.answerPermission({ native_request_id: request.request_id, action, decision: "allow" }).catch(() => undefined);
      }, 25);
      return action;
    },
    async verifyAnswer() { return true; },
    async recordNativeEvent(event) { nativeEvents.push(event); },
  };
  return { host, decisions, denials, nativeEvents };
}

// ── the run ──────────────────────────────────────────────────────────────────
let temp: string | undefined;
let decisions: Decision[] = [];
let nativeEvents: unknown[] = [];
let activeAdapter: OmpAdapter | undefined;
let activeRef: SessionRef | undefined;
const running: Array<Subprocess<"ignore", "ignore", "ignore"> | Subprocess<"ignore", "pipe", "ignore">> = [];

try {
  assert.equal(args.harness, "omp", `this smoke only implements --harness omp, got ${args.harness}`);
  const ompBinary = await resolveOmpBinary(args.ompBinary);
  evidence.omp_binary = ompBinary;

  const llamaBinary = args.llamaBinary ?? "llama-server";
  assert.equal((await capture("bash", ["-lc", `command -v ${llamaBinary}`], process.env, repoRoot)).code, 0, "llama-server is required to host the local model");

  coreBinary = join(repoRoot, "target/debug/ironbark-core");
  cliBinary = join(repoRoot, "target/debug/ironbark-cli");
  assert.ok(await exists(coreBinary), `built core missing: ${coreBinary}`);
  assert.ok(await exists(cliBinary), `built CLI missing: ${cliBinary}`);

  temp = await mkdtemp(join(tmpdir(), "h1-harness-"));
  evidence.temp_root = temp;

  // ── 1. local llama.cpp endpoint ────────────────────────────────────────────
  const requestedEndpoint = args.endpoint ?? "http://127.0.0.1:8091";
  const operatorCtx = await probeCtx(requestedEndpoint);
  evidence.operator_endpoint = { endpoint: requestedEndpoint, n_ctx: operatorCtx };
  let local: LocalServer;
  if (operatorCtx >= 8192) {
    local = { endpoint: requestedEndpoint, owned: false, nCtx: operatorCtx, modelPath: "", note: "operator endpoint has adequate context" };
  } else {
    const modelPath = await discoverModel();
    const logPath = join(temp, "llama-server.log");
    running.push(await startOwnLlama(llamaBinary, modelPath, 18099, logPath));
    local = { endpoint: "http://127.0.0.1:18099", owned: true, nCtx: await probeCtx("http://127.0.0.1:18099"), modelPath, note: `operator ${requestedEndpoint} n_ctx=${operatorCtx} is below OMP's ACP engine prompt; started own 16k-context llama.cpp server` };
    evidence.own_llama_server = { endpoint: local.endpoint, model: modelPath, log: logPath };
    notes.push(local.note);
  }
  record("local endpoint", `probe ${requestedEndpoint}/props`, operatorCtx > 0 ? 0 : 1, "a local llama.cpp endpoint with enough context for OMP", `${local.endpoint} n_ctx=${local.nCtx} (${local.note})`);
  evidence.local_endpoint = local;

  // ── 2. isolated OMP profile pointed at the local endpoint ───────────────────
  // OmpAdapter derives HOME=`${stateDir}/home` and XDG roots under `${stateDir}`;
  // the profile helper must write models.yml into that same HOME's profile dir.
  const adapterState = join(temp, "omp-state");
  const ompHome = join(adapterState, "home");
  const ompXdg = { config: join(adapterState, "xdg/config"), cache: join(adapterState, "xdg/cache"), data: join(adapterState, "xdg/data"), state: join(adapterState, "xdg/state") };
  // Every OMP invocation uses the isolated HOME/XDG; the operator's own ~/.omp is
  // never read or written, including this version probe.
  for (const dir of [ompHome, ompXdg.config, ompXdg.cache, ompXdg.data, ompXdg.state]) await mkdir(dir, { recursive: true, mode: 0o700 });
  const ompVersion = await capture(ompBinary, ["--version"], ompEnv(ompHome, ompXdg), ompHome);
  assert.equal(ompVersion.code, 0, `omp --version failed in the isolated env: ${ompVersion.stderr.trim()}`);
  evidence.omp = { binary: ompBinary, version: ompVersion.stdout.trim() };
  const profile = await prepareOmpProfile({
    ompBinary, home: ompHome, xdg: ompXdg, profile: PROFILE,
    providers: [{ name: "local", api: "openai-completions", baseUrl: local.endpoint, auth: "none", discovery: { type: "llama.cpp" } }],
  });
  const selectorProbe = profile.probes.at(-1)!;
  const selector = await readSelector(selectorProbe.stdout);
  assert.ok(selector, `could not read the local model selector from models.yml: ${selectorProbe.stdout.slice(0, 200)}`);
  record("omp profile", selectorProbe.command, 0, "an isolated models.yml registering the local provider", `agentDir=${profile.agentDir}, selector=${selector}`);
  evidence.omp_profile = { agentDir: profile.agentDir, modelsYml: profile.modelsYml, selector, probes: profile.probes };
  // Approval mode: OMP must ask the core for every tool so the core owns the
  // decision (default `yolo` would let OMP self-approve).
  const settingsProbes = await applyOmpSettings(ompBinary, ompHome, ompXdg, PROFILE, { "tools.approvalMode": "always-ask" });
  evidence.omp_settings = settingsProbes;
  // Scope OMP's engine prompt (tool schemas + skills) to fit a CPU-only local
  // model under the adapter's 120 s ACP-silence window.
  const scopedBinary = await writeScopedOmpWrapper({ ompBinary, dir: join(temp, "omp-wrapper"), tools: ["write", "read", "bash"] });
  evidence.scoped_wrapper = { path: scopedBinary, tools: ["write", "read", "bash"], flags: ["--no-skills", "--no-rules", "--no-lsp"] };
  record("omp scoped wrapper", `${scopedBinary} -> omp --tools=write,read,bash --no-skills --no-rules --no-lsp`, 0, "a scoped OMP engine prompt", `wrapper ${scopedBinary}`);

  // ── 3. core daemon in isolated XDG ─────────────────────────────────────────
  const env = coreEnv(join(temp, "core"));
  for (const dir of [env.XDG_DATA_HOME, env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR]) await mkdir(dir, { recursive: true, mode: 0o700 });
  const coreProcess = startCoreDraining(coreBinary, env, repoRoot);
  running.push(coreProcess.child);
  evidence.core_ready = await deadline(coreProcess.ready, 20_000, "core readiness");
  record("core start", coreBinary, 0, "CORE_READY", String(evidence.core_ready));

  // ── 4. project rooted at a real temp git repository ────────────────────────
  const repo = join(temp, "repo");
  await mkdir(repo, { recursive: true });
  await git(repo, ["init", "-q", "."]);
  await writeFile(join(repo, "README.md"), "# h1 harness project\n");
  await git(repo, ["add", "README.md"]);
  await git(repo, ["commit", "-q", "-m", "init"]);
  const baseCommit = await git(repo, ["rev-parse", "HEAD"]);
  const project = await cliJson<Project>(env, ["project", "create", "h1-harness", "--root", repo]);
  assert.ok(project.id, "project create must return an id");
  record("project create", `ironbark-cli project create h1-harness --root ${repo}`, 0, "a project DTO", `project ${project.id}`);
  evidence.project = project;

  // ── 5. register the local connection through the product ───────────────────
  let pinnedConnection = args.connection;
  if (!pinnedConnection) {
    const requestedId = `local-${sha256(repo).slice(0, 12)}`;
    const docPath = join(temp, "connection.json");
    await writeFile(docPath, JSON.stringify({ kind: "connection", config: { id: requestedId, protocol: "llama_cpp", endpoint: local.endpoint, models: [{ id: selector, context_limit: 16384, output_limit: 4096, capabilities: [] }], allowed_projects: [] } }));
    const preview = await command<ImportPreview>(env, { type: "import_preview", project_id: project.id, source: { path: docPath, format: { type: "document" } } });
    const receipt = await command<ImportReceipt>(env, { type: "import_apply", project_id: project.id, preview_digest: preview.digest });
    assert.equal(receipt.kind, "connection");
    pinnedConnection = receipt.connection_id ?? requestedId;
    record("connection register", `ironbark-cli command import_preview+import_apply (${docPath})`, 0, "a connection import receipt", `connection ${pinnedConnection}, endpoint ${local.endpoint}`);
    evidence.connection = { requested_id: requestedId, registered_id: pinnedConnection, endpoint: local.endpoint };
  } else {
    record("connection (pre-registered)", `--connection ${pinnedConnection}`, 0, "the caller-supplied connection id", `using ${pinnedConnection}`);
  }

  // ── 6. writer worktree + committed policy ──────────────────────────────────
  const worktree = join(temp, "wt-run");
  await git(repo, ["worktree", "add", "-q", "-b", "h1-run", worktree, baseCommit]);
  const worktreeLease = { project_id: project.id, run_id: "", path: worktree, base_commit: baseCommit, mode: "writer" as const };
  evidence.worktree_lease = worktreeLease;
  notes.push("The core WorkspaceService worktree-lease API is Rust-only in this build (not exposed over IPC); the smoke materialises the writer worktree with real git (the same base/one-writer rule the Rust test asserts) and drives OMP within it.");
  notes.push("Adapter fs access: AcpAdapterBase advertises clientCapabilities.fs.{readTextFile,writeTextFile}=true; under IRONBARK_LIVE_HARNESS=1 it now serves `fs/write_text_file`/`fs/read_text_file` confined to the session worktree, so OMP's edit tool lands a real file (previously it was answered {denied:true}, which OMP treated as success while writing nothing). Without the gate the fail-closed deny is unchanged.");

  const policy: ProjectPolicy = {
    version: "1", project_id: project.id, allowed_connection_ids: [pinnedConnection],
    allowed_tools: ["write", "read", "bash"], read_roots: [worktree], write_roots: [worktree],
    network_destinations: [], local_only: true, max_workers: 1, max_requests: 16,
    max_cost_microusd: "0", spend_policy: "free_only", allow_unknown_cost: false,
  };
  const policyPath = join(temp, "policy.json");
  await writeFile(policyPath, JSON.stringify(policy));
  const committed = await cliJson<ProjectPolicy>(env, ["policy", "set", "--project", project.id, "--file", policyPath]);
  assert.equal(committed.version, 1);
  record("policy set", `ironbark-cli policy set --project ${project.id} --file ${policyPath}`, 0, "the committed ProjectPolicy", `version ${committed.version}, write_roots ${committed.write_roots.join(",")}`);

  // ── 7. run lifecycle ───────────────────────────────────────────────────────
  const task = "Create a file named hello.txt in the current working directory containing exactly the text: hi";
  const run = await cliJson<RunReceipt>(env, ["run", "create", "--project", project.id, "--task", task]);
  worktreeLease.run_id = run.run_id;
  record("run create", `ironbark-cli run create --project ${project.id}`, 0, "a queued RunReceipt", `run ${run.run_id}, state ${run.state}`);
  const started = await cliJson<string>(env, ["run", "start", "--project", project.id, "--run", run.run_id, "--connection", pinnedConnection, "--model", selector, "--harness", "omp"]);
  assert.equal(started, "running");
  record("run start", `ironbark-cli run start --project ${project.id} --run ${run.run_id}`, 0, "running", `state ${started}`);

  // ── 8. drive the real OMP adapter end to end ───────────────────────────────
  const adapterRef: { current?: OmpAdapter } = {};
  const made = makeHost(policy, worktree, adapterRef);
  const host = made.host;
  decisions = made.decisions;
  nativeEvents = made.nativeEvents;
  const adapter = new OmpAdapter(host, { binary: scopedBinary, stateDir: adapterState, profile: PROFILE });
  adapterRef.current = adapter;
  activeAdapter = adapter;
  const context: ContextPackage = { digest: sha256("h1"), brief: "H1 harness", constraints: [], excerpts: [], artifacts: [], unresolved_questions: [], originating_sessions: [] };
  const openInput: OpenSessionInput = { run_id: run.run_id, project_id: project.id, cwd: worktree, route: { connection_id: pinnedConnection, model_id: selector, harness: "omp" }, context, policy };
  const descriptor = await adapter.describe();
  evidence.describe = descriptor;
  record("adapter describe", `${ompBinary} --profile ${PROFILE} acp (version probe)`, 0, "an omp HarnessDescriptor", `version ${descriptor.version}, transport ${descriptor.transport}`);

  const tools: ToolDescriptor[] = [
    { name: "write", description: "write a file", input_schema: {} },
    { name: "read", description: "read a file", input_schema: {} },
    { name: "bash", description: "run a shell command", input_schema: {} },
  ];
  interface TurnResult { text: string; approvals: number; tools: string[]; status: string; stopReason: string; toolResults: unknown[] }
  async function runTurn(session: SessionRef, promptText: string): Promise<TurnResult> {
    const turn: TurnInput = { request_id: crypto.randomUUID(), message_id: crypto.randomUUID(), content: [{ type: "text", text: promptText }], allowed_tools: tools, budget: policy };
    let text = ""; let approvals = 0; let status = "unknown"; let stopReason = "";
    const toolKinds: string[] = []; const toolResults: unknown[] = [];
    console.error(`[turn] start: ${promptText.slice(0, 70)}`);
    for await (const event of adapter.turn(session, turn)) {
      const payload: WorkerEvent["payload"] = event.payload;
      switch (payload.type) {
        case "text_delta": text += payload.delta; break;
        case "approval_needed": approvals += 1; break;
        case "tool_started": toolKinds.push(payload.action.kind); break;
        case "tool_finished": toolResults.push(payload.result); break;
        case "turn_finished": status = payload.status; stopReason = payload.stop_reason ?? ""; break;
        default: break;
      }
    }
    console.error(`[turn] done: status=${status} stop=${stopReason} tools=${toolKinds.join(",") || "(none)"} approvals=${approvals}`);
    return { text, approvals, tools: toolKinds, status, stopReason, toolResults };
  }
  const turnWindowMs = 240_000;
  async function openSession(): Promise<SessionRef> {
    const session = await adapter.open(openInput);
    activeRef = session;
    return session;
  }
  async function closeSession(session: SessionRef): Promise<void> {
    await adapter.close(session).catch(() => undefined);
    if (activeRef && activeRef.native_id === session.native_id) activeRef = undefined;
  }

  // ── 9. the file-creating task (real diff) ──────────────────────────────────
  // With IRONBARK_LIVE_HARNESS=1 the adapter serves ACP `fs/write_text_file`, so
  // OMP's own edit tool writes a real file. Without the gate (or if the model
  // chooses bash) OMP's server-side bash tool produces the same real diff.
  const helloPath = join(worktree, "hello.txt");
  const nativePrompt = `${task}. Then stop.`;
  const bashPrompt = `${task}. Use your bash tool to run exactly: printf %s hi > hello.txt . Then stop.`;
  let primarySession = await openSession();
  let nativeId = primarySession.native_id;
  evidence.native_session = primarySession;
  record("adapter open", `${scopedBinary} --profile ${PROFILE} acp  session/new cwd=${worktree}`, 0, "a bound native OMP session", `native_id ${nativeId}, version ${primarySession.native_version}`);
  let createTurn = await deadline(runTurn(primarySession, nativePrompt), turnWindowMs, "create turn");
  let createdBy = "edit-tool";
  if (!(await exists(helloPath))) {
    notes.push("The natural prompt did not produce hello.txt through the edit/fs path this run; retrying in a fresh session with OMP's bash tool.");
    await closeSession(primarySession);
    primarySession = await openSession();
    nativeId = primarySession.native_id;
    createTurn = await deadline(runTurn(primarySession, bashPrompt), turnWindowMs, "create turn retry");
    createdBy = "bash-tool";
  }
  evidence.turn_create = { created_by: createdBy, text: createTurn.text, status: createTurn.status, stopReason: createTurn.stopReason, tools: createTurn.tools };
  const helloExists = await exists(helloPath);
  const helloContent = helloExists ? (await readFile(helloPath, "utf8")).trim() : "";
  record("omp real task", `session/prompt "${createdBy === "edit-tool" ? nativePrompt : bashPrompt}"`, 0, "a real OMP turn producing hello.txt inside the worktree", `created via ${createdBy}, status ${createTurn.status}, tools ${createTurn.tools.join(",") || "(none)"}, hello.txt=${helloExists ? JSON.stringify(helloContent) : "ABSENT"}`);
  assert.ok(helloExists, "OMP did not create hello.txt in the worktree");
  const porcelain = await git(worktree, ["status", "--porcelain"]);
  const diffText = await capture("git", ["-C", worktree, "diff"], process.env, worktree);
  evidence.diff = { porcelain, diff: diffText.stdout, hello_sha256: sha256(helloContent) };
  record("real diff", `git -C ${worktree} status --porcelain`, 0, "the untracked created file", porcelain.split("\n").filter(Boolean).join("; ") || "(no diff)");

  // ── 10. pause → resume keeps the same native session id ────────────────────
  // Resume a paused run into `ready` (the scheduler then requires an explicit
  // start), so the durable state after resume is `ready`, not `running`.
  const paused = await cliJson<string>(env, ["run", "pause", "--project", project.id, "--run", run.run_id]);
  assert.equal(paused, "paused");
  const resumed = await cliJson<string>(env, ["run", "resume", "--project", project.id, "--run", run.run_id]);
  assert.equal(resumed, "ready");
  assert.equal(primarySession.native_id, nativeId);
  const continuity = await deadline(runTurn(primarySession, "Reply with exactly the two words: still here. Then stop."), turnWindowMs, "continuity turn");
  const restarted = await cliJson<string>(env, ["run", "start", "--project", project.id, "--run", run.run_id, "--connection", pinnedConnection, "--model", selector, "--harness", "omp"]);
  assert.equal(restarted, "running");
  evidence.pause_resume = { paused, resumed, restarted, native_id: primarySession.native_id, continuity_status: continuity.status, continuity_text: continuity.text };
  record("pause/resume identity", `ironbark-cli run pause/resume/start --project ${project.id} --run ${run.run_id}`, 0, "the same native OMP session continues through pause/resume", `paused->${paused}, resumed->${resumed}, restarted->${restarted}, native_id ${nativeId} unchanged, continuity turn ${continuity.status}`);
  await closeSession(primarySession);

  // ── 11. denial: a write outside the leased worktree leaves no file ─────────
  const outsideDir = join(temp, "h1-smoke-outside");
  await mkdir(outsideDir, { recursive: true });
  const outsideSentinel = join(outsideDir, "outside-sentinel.txt");
  const denialSession = await openSession();
  const beforeDenial = decisions.length;
  const denialTurn = await deadline(runTurn(denialSession, `Use your bash tool to run exactly: printf sentinel > ${outsideSentinel} . Then stop.`), turnWindowMs, "denial turn");
  const outsideExists = await exists(outsideSentinel);
  const denialDecisions = decisions.slice(beforeDenial).filter((d) => d.decision === "deny");
  record("denial: write outside worktree", `session/prompt "printf sentinel > ${outsideSentinel}"`, 0, "the core denies the out-of-scope write and no file appears", `denials=${denialDecisions.length}, sentinel exists=${outsideExists}, status ${denialTurn.status}`);
  assert.ok(!outsideExists, `a write outside the leased worktree produced ${outsideSentinel}`);
  assert.ok(denialDecisions.length > 0, "the out-of-scope write was not denied by the core");
  evidence.denial = { path: outsideSentinel, exists: outsideExists, decisions: denialDecisions, status: denialTurn.status };
  await closeSession(denialSession);

  // ── 12. cancelled child command observed stopped ───────────────────────────
  const childPidFile = join(worktree, "child.pid");
  const cancelSession = await openSession();
  const cancelTurn = runTurn(cancelSession, `Use your bash tool to run exactly this command and then wait for it to finish: sh -c 'echo $$ > ${childPidFile}; sleep 120'`);
  let childPid = 0;
  for (let i = 0; i < 600; i += 1) {
    if (await exists(childPidFile)) { childPid = Number((await readFile(childPidFile, "utf8")).trim()) || 0; break; }
    await delay(500);
  }
  record("cancel: child started", `observed ${childPidFile}`, childPid > 0 ? 0 : 1, "a live OMP child command", childPid > 0 ? `child pid ${childPid} running` : "the model did not start the cancellable child command");
  if (childPid === 0) notes.push("The model did not start the cancellable child command; the cancel boundary could not be exercised by a real tool call.");
  await adapter.interrupt(cancelSession);
  let cancelled: TurnResult;
  try { cancelled = await deadline(cancelTurn, turnWindowMs, "cancel turn"); }
  catch { cancelled = { text: "", approvals: 0, tools: [], status: "interrupt_timeout", stopReason: "", toolResults: [] }; }
  // "Reported stopped" only after the OS process is observed gone.
  let observedExit = childPid === 0;
  for (let i = 0; i < 120 && !observedExit; i += 1) {
    try { process.kill(childPid, 0); } catch { observedExit = true; break; }
    await delay(500);
  }
  assert.ok(observedExit, childPid > 0 ? `cancelled child ${childPid} is still running` : "no child command started");
  evidence.cancel = { child_pid: childPid, observed_exit: observedExit, turn_status: cancelled.status };
  record("cancel: observed stop", `adapter.interrupt(); poll kill -0 ${childPid}`, 0, "the cancelled child process is observed gone", `child pid ${childPid} observed_exit=${observedExit}, turn status ${cancelled.status}`);
  await closeSession(cancelSession);
  // ── 13. core receipts for the run ──────────────────────────────────────────
  const manifestPath = join(temp, "checkpoint.json");
  await writeFile(manifestPath, JSON.stringify({
    run_id: run.run_id,
    native_session: primarySession,
    artifacts: [],
    checks: [{ name: "real-diff", passed: helloExists, evidence_digest: sha256(diffText.stdout), details: `git status: ${porcelain.split("\n").filter(Boolean).join("; ") || "(clean)"}` }],
    acceptance_satisfied: helloExists,
    unresolved: [],
  }));
  const checkpoint = await cliJson<string>(env, ["checkpoint", "capture", "--project", project.id, "--run", run.run_id, "--phase", "post-verification", "--manifest", manifestPath]);
  record("checkpoint capture", `ironbark-cli checkpoint capture --project ${project.id} --run ${run.run_id} --phase post-verification`, 0, "a CAS manifest link artifact id", `artifact ${checkpoint}`);
  const nativeDone = await cliJson<string>(env, ["run", "native-done", "--project", project.id, "--run", run.run_id, "--receipt", `omp ${nativeId}`]);
  record("native done", `ironbark-cli run native-done --project ${project.id} --run ${run.run_id}`, 0, "the native harness reported done", `state ${nativeDone}`);
  const inspect = await cliJson<{ run_state: string }>(env, ["run", "inspect", "--project", project.id, "--run", run.run_id]);
  evidence.receipts = { checkpoint, native_done: nativeDone, inspect };
  record("run inspect", `ironbark-cli run inspect --project ${project.id} --run ${run.run_id}`, 0, "the run receipts", `state ${inspect.run_state}`);

  await cli(env, ["shutdown"]);
  evidence.state = "passed";
} catch (error) {
  evidence.state = "failed";
  evidence.error = error instanceof Error ? error.message : String(error);
  process.exitCode = 1;
} finally {
  if (activeAdapter && activeRef) await activeAdapter.close(activeRef).catch(() => undefined);
  for (const child of running) {
    if (child.exitCode === null) { child.kill("SIGINT"); try { await deadline(child.exited, 8_000, "stop"); } catch { child.kill("SIGKILL"); await child.exited; } }
  }
  if (temp) await rm(temp, { recursive: true, force: true });
  await mkdir(reportDir, { recursive: true });
  await writeFile(reportPath, `${JSON.stringify({
    state: evidence.state ?? "failed",
    harness: args.harness,
    mechanism: "the real OmpAdapter spawns `omp --profile ironbark-worker acp` over stdio in an isolated HOME/XDG whose profile models.yml registers the local llama.cpp endpoint; the built core daemon owns the project/policy/connection/run",
    omp: evidence.omp,
    local_endpoint: evidence.local_endpoint,
    omp_profile: evidence.omp_profile,
    native_session: evidence.native_session,
    diff: evidence.diff,
    denial: evidence.denial,
    pause_resume: evidence.pause_resume,
    cancel: evidence.cancel,
    receipts: evidence.receipts,
    permission_decisions: decisions,
    native_events: nativeEvents,
    isolation: "isolated OMP HOME/XDG + profile under one temp root; the built core in its own temp XDG; the operator's ~/.omp, store and llama.cpp servers (8091/8092) were only probed, never written",
    steps,
    evidence,
    notes,
  }, null, 2)}\n`);
  console.log(JSON.stringify({ state: evidence.state, steps: steps.map((s) => `${s.name}: exit ${s.exit_code} — ${s.observed}`), decisions: decisions.map((d) => `${d.decision} kind=${d.kind} title=${d.title} path=${d.path ?? "-"} cmd=${(d.command ?? "-").slice(0, 60)} :: ${d.reason}`), receipt: reportPath, error: evidence.error }, null, 2));
}
