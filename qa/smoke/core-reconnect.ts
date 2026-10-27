import { strict as assert } from "node:assert";
import { mkdtemp, mkdir, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createConnection, type Socket } from "node:net";
import type { Subprocess } from "bun";
import type { ClientFrame, CoreCommand, CoreReply, ServerFrame } from "../../packages/contracts/src/generated";

// Requires parent-built binaries; never builds or replaces them under this smoke.
const daemonBinary = resolve(process.env.IRONBARK_CORE_BIN ?? "target/debug/ironbark-core");
const cliBinary = resolve(process.env.IRONBARK_CLI_BIN ?? "target/debug/ironbark-cli");
const temp = await mkdtemp(join(tmpdir(), "ibc1-"));
const env = { ...process.env, HOME: temp, XDG_DATA_HOME: join(temp, "d"), XDG_STATE_HOME: join(temp, "s"), XDG_RUNTIME_DIR: join(temp, "r") };
for (const dir of [env.XDG_DATA_HOME, env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR]) await mkdir(dir, { mode: 0o700 });
const runtime = join(env.XDG_RUNTIME_DIR, "ironbark-assistant");
const endpoint = join(runtime, "core.sock");
let daemon: Subprocess<"ignore", "pipe", "pipe"> | undefined;
const connections = new Set<Wire>();

function deadline<T>(promise: Promise<T>, milliseconds = 10_000): Promise<T> {
  const expired = Promise.withResolvers<never>();
  const timer = setTimeout(() => expired.reject(new Error("core smoke deadline exceeded")), milliseconds);
  return Promise.race([promise, expired.promise]).finally(() => clearTimeout(timer));
}
class Wire {
  readonly socket: Socket;
  private buffer = Buffer.alloc(0);
  private queue: ServerFrame[] = [];
  private waiter?: { resolve: (frame: ServerFrame) => void; reject: (error: Error) => void };
  private ended = false;
  private failure?: Error;
  constructor(socket: Socket) {
    this.socket = socket;
    socket.on("data", (data: Buffer) => {
      this.buffer = Buffer.concat([this.buffer, data]);
      if (this.buffer.length > 1024 * 1024 + 1) { this.fail(new Error("oversized server frame")); return; }
      let index: number;
      while ((index = this.buffer.indexOf(10)) >= 0) {
        let frame: ServerFrame;
        try { frame = JSON.parse(this.buffer.subarray(0, index).toString("utf8")) as ServerFrame; }
        catch { this.fail(new Error("invalid server frame")); return; }
        this.buffer = this.buffer.subarray(index + 1);
        if (this.waiter) { const waiter = this.waiter; this.waiter = undefined; waiter.resolve(frame); }
        else { this.queue.push(frame); if (this.queue.length > 256) this.fail(new Error("client smoke queue overflow")); }
      }
    });
    socket.on("error", (error) => this.fail(error));
    socket.on("close", () => { this.ended = true; if (this.waiter) { this.waiter.reject(new Error("core connection closed")); this.waiter = undefined; } });
    connections.add(this);
  }
  private fail(error: Error): void { this.failure = error; this.waiter?.reject(error); this.waiter = undefined; this.close(); }
  close(): void { connections.delete(this); this.socket.destroy(); }
  send(frame: ClientFrame): void { this.socket.write(JSON.stringify(frame) + "\n"); }
  async next(): Promise<ServerFrame> {
    const frame = this.queue.shift(); if (frame) return frame;
    if (this.failure) throw this.failure;
    if (this.ended) throw new Error("core connection closed");
    assert.equal(this.waiter, undefined, "only one frame consumer permitted");
    const pending = Promise.withResolvers<ServerFrame>();
    this.waiter = pending;
    return deadline(pending.promise);
  }
  static async connect(nonce: string): Promise<Wire> {
    const socket = createConnection(endpoint);
    const wire = new Wire(socket);
    const connected = Promise.withResolvers<void>();
    socket.once("connect", connected.resolve);
    socket.once("error", connected.reject);
    await deadline(connected.promise);
    wire.send({ type: "hello", protocol: 1, session_nonce: nonce });
    return wire;
  }
  async request(id: string, command: CoreCommand): Promise<CoreReply> {
    this.send({ type: "request", request_id: id, command });
    const frame = await this.next();
    assert.equal(frame.type, "reply");
    if (frame.type !== "reply") throw new Error("not a reply");
    assert.equal(frame.request_id, id);
    return frame.reply;
  }
}
async function start(): Promise<void> {
  daemon = Bun.spawn([daemonBinary], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const stream = daemon.stdout;
  assert(stream && typeof stream !== "number");
  const reader = stream.getReader();
  await deadline((async () => {
    let text = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) throw new Error("actual daemon exited before readiness");
      text += new TextDecoder().decode(value);
      if (text.includes("CORE_READY protocol=1")) { reader.releaseLock(); return; }
    }
  })());
}
async function authenticated(): Promise<Wire> {
  const wire = await Wire.connect((await readFile(join(runtime, "session.nonce"), "utf8")).trim());
  assert.deepEqual(await wire.next(), { type: "hello", protocol: 1 });
  return wire;
}
function ok(reply: CoreReply): unknown { if (reply.type !== "ok") throw new Error(reply.code); return reply.value.value; }
async function stop(requestId = crypto.randomUUID()): Promise<void> {
  assert.deepEqual(JSON.parse(await cli(["--request-id", requestId, "shutdown"])), { shutting_down: true });
  assert.equal(await deadline(daemon!.exited), 0);
  daemon = undefined;
}
async function cli(args: string[]): Promise<string> {
  const child = Bun.spawn([cliBinary, ...args], { env, stdout: "pipe", stderr: "pipe" });
  try {
    const text = await deadline(new Response(child.stdout).text());
    assert.equal(await deadline(child.exited), 0, `actual CLI failed: ${args.join(" ")}`);
    return text;
  } finally {
    if (child.exitCode === null) { child.kill("SIGKILL"); await child.exited; }
  }
}
try {
  await start();
  const rivalRuntime = join(temp, "rival-runtime");
  await mkdir(rivalRuntime, { mode: 0o700 });
  const rival = Bun.spawn([daemonBinary], {
    env: { ...env, XDG_RUNTIME_DIR: rivalRuntime },
    stdin: "ignore", stdout: "pipe", stderr: "pipe",
  });
  try {
    assert.equal(await deadline(rival.exited), 1, "same-store second authority must fail before serving");
    const rivalError = await new Response(rival.stderr).text();
    assert(rivalError.includes("core_already_running"), "second authority must fail on the data lock");
  } finally {
    if (rival.exitCode === null) { rival.kill("SIGKILL"); await rival.exited; }
  }
  const command: CoreCommand = { type: "project_create", name: "reconnect-boundary", root: temp };
  const requestId = crypto.randomUUID();
  const first = await authenticated();
  const original = await first.request(requestId, command);
  const project = ok(original) as { id: string };
  assert.match(project.id, /^[0-9a-f-]{36}$/);
  first.close();
  const subscriber = await authenticated();
  subscriber.send({ type: "subscribe", after_seq: "0" });
  const committed = await subscriber.next();
  assert.equal(committed.type, "event");
  if (committed.type !== "event") throw new Error("missing committed event");
  assert.equal(committed.event.seq, "1");
  assert.equal(committed.event.request_id, requestId);
  subscriber.close();
  const reconnect = await authenticated();
  assert.deepEqual(await reconnect.request(requestId, command), original);
  const conflict = await reconnect.request(requestId, { ...command, name: "different" });
  assert.equal(conflict.type, "error");
  if (conflict.type === "error") assert.equal(conflict.code, "request_id_conflict");
  reconnect.close();
  const bad = await Wire.connect("intentionally-incorrect-nonce");
  const denied = await bad.next();
  assert.equal(denied.type, "error");
  if (denied.type === "error") assert.equal(denied.code, "bad_nonce");
  bad.close();
  const cliProject = JSON.parse(await cli(["--request-id", requestId, "project", "create", "reconnect-boundary", "--root", temp]));
  assert.deepEqual(cliProject, ok(original));
  const status = JSON.parse(await cli(["status"]));
  assert.equal(status.projects, "1");
  assert.equal(status.last_seq, "1");
  const doctor = JSON.parse(await cli(["doctor", "--json"]));
  assert.equal(doctor.sqlite_integrity, "ok");
  assert.equal(doctor.foreign_keys, true);
  assert.equal(doctor.journal_mode, "wal");
  for (const path of [join(env.XDG_DATA_HOME, "ironbark-assistant"), join(env.XDG_STATE_HOME, "ironbark-assistant"), runtime]) assert.equal((await stat(path)).mode & 0o777, 0o700);
  for (const path of [join(runtime, "session.nonce"), endpoint, join(env.XDG_DATA_HOME, "ironbark-assistant/core.sqlite3")]) assert.equal((await stat(path)).mode & 0o777, 0o600);
  const shutdownId = crypto.randomUUID();
  await stop(shutdownId);
  await start();
  const restarted = await authenticated();
  assert.deepEqual(ok(await restarted.request(shutdownId, { type: "shutdown" })), { shutting_down: true });
  const replayStatus = ok(await restarted.request(crypto.randomUUID(), { type: "status" }));
  assert(replayStatus !== null && typeof replayStatus === "object" && "projects" in replayStatus);
  assert.equal(replayStatus.projects, "1", "replayed shutdown must leave the replacement daemon serving");
  assert.deepEqual(await restarted.request(requestId, command), original);
  const projects = ok(await restarted.request(crypto.randomUUID(), { type: "project_list" })) as unknown[];
  assert.equal(projects.length, 1);
  assert.deepEqual(JSON.parse(await cli(["project", "list"])), projects);
  const events = ok(await restarted.request(crypto.randomUUID(), { type: "events", after: "0" })) as { seq: string }[];
  assert.deepEqual(events.map((event) => event.seq), ["1"]);
  restarted.close();
  const cliEvents = (await cli(["events", "--after", "0"])).trim().split("\n").map((line) => JSON.parse(line));
  assert.deepEqual(cliEvents.map((event) => event.seq), ["1"]);
  await stop();
  await rm(temp, { recursive: true, force: true });
  const observation = { protocol: 1, daemon: "actual prebuilt Rust binary", cli: "actual prebuilt Rust binary", projects: projects.length, committed_sequences: events.map((event) => event.seq), identical_request_replay: "same durable reply", mismatched_request_replay: "rejected", bad_nonce: "rejected", restart: "durable", sqlite_integrity: doctor.sqlite_integrity, private_permissions: "0700 directories / 0600 files and socket", temporary_state: "removed after smoke", credentials: "none" };
  const report = ".superpowers/sdd/2026-10-12-ironbark-assistant/C1-smoke.json";
  await Bun.write(report, JSON.stringify(observation, null, 2) + "\n");
  console.log(JSON.stringify(observation));
} finally {
  for (const wire of connections) wire.close();
  if (daemon) { daemon.kill("SIGINT"); try { await deadline(daemon.exited, 5000); } catch { daemon.kill("SIGKILL"); await daemon.exited; } }
  await rm(temp, { recursive: true, force: true });
}
