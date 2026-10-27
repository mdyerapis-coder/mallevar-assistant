// D1 desktop-connect smoke.
//
// Proves the desktop's native IPC end-to-end against a real core:
//   1. Boots an isolated daemon (fresh XDG runtime dir + temp HOME), so the
//      operator's live daemon is never touched.
//   2. Drives the exact wire protocol the Tauri commands wrap — hello/nonce
//      auth, request-id correlation, dedicated subscribe socket with durable
//      pull replay — for the four operations the GUI dispatches
//      (Status / Doctor / ProjectList / ProjectCreate + the event stream).
//   3. When a display is present, launches the ACTUAL Tauri window binary
//      against the same isolated daemon and confirms (a) a WebKitWebProcess
//      child of the window stays alive and (b) the daemon and the window share
//      an ESTAB unix-socket connection — the live-GUI proof that the webview
//      authenticated to the core and opened its sockets. No screenshot: this
//      host is Wayland with no capture surface.
import { strict as assert } from "node:assert";
import { readlinkSync, readdirSync } from "node:fs";
import { mkdtemp, mkdir, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createConnection, type Socket } from "node:net";
import type { Subprocess } from "bun";
import type { ClientFrame, CoreCommand, CoreReply, ServerFrame } from "../../packages/contracts/src/generated";
const temp = await mkdtemp(join(tmpdir(), "ibd1-"));
const daemonBinary = resolve(process.env.IRONBARK_CORE_BIN ?? "target/debug/ironbark-core");
const desktopBinary = resolve(process.env.IRONBARK_DESKTOP_BIN ?? "target/debug/ironbark-desktop");
const desktopRoot = resolve("apps/desktop");
const env = {
  ...process.env,
  HOME: temp,
  XDG_DATA_HOME: join(temp, "d"),
  XDG_STATE_HOME: join(temp, "s"),
  XDG_RUNTIME_DIR: join(temp, "r"),
};
for (const dir of [env.XDG_DATA_HOME, env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR]) await mkdir(dir, { mode: 0o700 });
const runtime = join(env.XDG_RUNTIME_DIR, "ironbark-assistant");
const endpoint = join(runtime, "core.sock");
let daemon: Subprocess<"ignore", "pipe", "pipe"> | undefined;
let devServer: Subprocess<"ignore", "pipe", "pipe"> | undefined;
let window: Subprocess<"ignore", "pipe", "pipe"> | undefined;
const connections = new Set<Wire>();

function deadline<T>(promise: Promise<T>, milliseconds = 10_000): Promise<T> {
  const expired = Promise.withResolvers<never>();
  const timer = setTimeout(() => expired.reject(new Error("desktop smoke deadline exceeded")), milliseconds);
  return Promise.race([promise, expired.promise]).finally(() => clearTimeout(timer));
}
async function sh(args: string[]): Promise<string> {
  const proc = Bun.spawn(args, { stdout: "pipe", stderr: "ignore" });
  const text = await deadline(new Response(proc.stdout).text());
  await proc.exited;
  return text;
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
    assert.deepEqual(await wire.next(), { type: "hello", protocol: 1 }, "core rejected desktop authentication");
    return wire;
  }
  async request(id: string, command: CoreCommand): Promise<CoreReply> {
    this.send({ type: "request", request_id: id, command });
    const frame = await this.next();
    assert.equal(frame.type, "reply", "expected a correlated reply");
    if (frame.type !== "reply") throw new Error("not a reply");
    assert.equal(frame.request_id, id, "reply request-id mismatch");
    return frame.reply;
  }
  // A subscription socket is dedicated: the core rejects commands on it. This
  // is the exact shape of CoreClient::subscribe the desktop uses for the
  // subscribe_events Tauri command.
  subscribe(afterSeq: string): void {
    this.send({ type: "subscribe", after_seq: afterSeq });
  }
  async nextEvent(): Promise<Extract<ServerFrame, { type: "event" }>["event"]> {
    const frame = await this.next();
    assert.equal(frame.type, "event", "subscription socket returned a non-event frame");
    if (frame.type !== "event") throw new Error("not an event");
    return frame.event;
  }
}
function ok(reply: CoreReply): unknown {
  if (reply.type !== "ok") throw new Error(`core ${reply.code}: ${reply.message}`);
  return reply.value.value;
}
async function nonce(): Promise<string> { return (await readFile(join(runtime, "session.nonce"), "utf8")).trim(); }
async function start(): Promise<void> {
  daemon = Bun.spawn([daemonBinary], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const stream = daemon.stdout;
  assert(stream && typeof stream !== "number");
  const reader = stream.getReader();
  await deadline((async () => {
    let text = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) throw new Error("daemon exited before readiness");
      text += new TextDecoder().decode(value);
      if (text.includes("CORE_READY protocol=1")) { reader.releaseLock(); return; }
    }
  })());
}
async function startDevServer(): Promise<void> {
  // A release binary ships a baked frontend, but a *debug* desktop binary resolves
  // `devUrl` (tauri.conf.json `build.devUrl` = http://localhost:5173). With no dev
  // server the webview loads a dead URL, JS never runs, and no core.sock opens —
  // which looks exactly like "no socket connection". Mirror `tauri dev`: start Vite
  // on 5173 (strict) under the isolated env first, then launch the window.
  devServer = Bun.spawn(["bunx", "vite", "--port", "5173", "--strictPort", "--host"], {
    env, stdin: "ignore", stdout: "pipe", stderr: "pipe", cwd: desktopRoot,
  });
  // Wait for Vite to print its ready banner (URL line) so the window loads.
  const ready = new RegExp("Local:.*localhost:5173");
  let text = "";
  const stream = devServer.stdout;
  assert(stream && typeof stream !== "number");
  const reader = stream.getReader();
  await deadline((async () => {
    while (true) {
      const { value, done } = await reader.read();
      if (done) throw new Error("dev server exited before readiness");
      text += new TextDecoder().decode(value);
      if (ready.test(text)) { reader.releaseLock(); return; }
    }
  })());
}
async function stop(): Promise<void> {
  for (const wire of connections) wire.close();
  window?.kill("SIGKILL");
  devServer?.kill("SIGKILL");
  daemon?.kill("SIGKILL");
  await Promise.all([
    window?.exited.catch(() => {}),
    devServer?.exited.catch(() => {}),
    daemon?.exited.catch(() => {}),
    rm(temp, { recursive: true, force: true }),
  ]);
}
// Socket inodes a process holds (ESTAB side of accepted connections shows up
// as an unnamed inode in /proc/<pid>/fd).
async function socketInodes(pid: number): Promise<Set<number>> {
  // In-process /proc read: one readlink per fd via the fs module, never a
  // spawned subprocess — this is polled every 500 ms in the window loop.
  const inodes = new Set<number>();
  let names: string[];
  try { names = readdirSync(`/proc/${pid}/fd`); } catch { return inodes; }
  for (const name of names) {
    let target: string;
    try { target = readlinkSync(`/proc/${pid}/fd/${name}`); } catch { continue; }
    const match = target.match(/^socket:\[(\d+)\]$/);
    if (match) inodes.add(Number(match[1]));
  }
  return inodes;
}
// The daemon's accepted connection inodes (excludes the listening socket).
// The listen inode is read from /proc/net/unix; everything else in the
// daemon's fd table that's a socket: is an accepted ESTAB connection.
async function daemonAcceptedInodes(): Promise<Set<number>> {
  assert(daemon, "daemon running");
  let listenInode: number | null = null;
  const lines = (await sh(["ss", "-x", "state", "listening"])).trim().split("\n").slice(1);
  for (const line of lines) {
    const tokens = line.trim().split(/\s+/);
    if (tokens.length < 5) continue;
    if (tokens[tokens.length - 1].includes("core.sock")) {
      listenInode = Number(tokens[tokens.length - 2]);
      break;
    }
  }
  const inodes = new Set<number>();
  let names: string[];
  try { names = readdirSync(`/proc/${daemon.pid}/fd`); } catch { return inodes; }
  for (const name of names) {
    let target: string;
    try { target = readlinkSync(`/proc/${daemon.pid}/fd/${name}`); } catch { continue; }
    const match = target.match(/^socket:\[(\d+)\]$/);
    if (match) {
      const ino = Number(match[1]);
      if (listenInode !== null && ino === listenInode) continue;
      inodes.add(ino);
    }
  }
  return inodes;
}
// The window shares an ESTAB unix connection with the daemon when the daemon
// holds one end (inode A) of an established pair and the window holds the
// other end (inode B); A !== B for a unix stream pair, so this must be tested
// through `ss -x` peer mapping, never by intersecting the two inode sets.
async function sharesUnixConnectionWithDaemon(windowPid: number): Promise<boolean> {
  assert(daemon, "daemon running");
  const windowInodes = await socketInodes(windowPid);
  const daemonInodes = await daemonAcceptedInodes();
  if (windowInodes.size === 0) return false;
  const lines = (await sh(["ss", "-x", "state", "established"])).trim().split("\n").slice(1);
  for (const line of lines) {
    const tokens = line.trim().split(/\s+/);
    // A unix stream row is 7 fields: Netid RecvQ SendQ LocalAddr LocalIno PeerAddr PeerIno.
    // For an established pair A (local ino) <-> B (peer ino), A and B are DISTINCT inodes:
    // one end is owned by the daemon, the other by the window.
    if (tokens.length !== 7) continue;
    const local = Number(tokens[4]);
    const peer = Number(tokens[6]);
    if (Number.isNaN(local) || Number.isNaN(peer)) continue;
    const daemonLocal = daemonInodes.has(local);
    const windowLocal = windowInodes.has(local);
    const daemonPeer = daemonInodes.has(peer);
    const windowPeer = windowInodes.has(peer);
    if ((daemonLocal && windowPeer) || (daemonPeer && windowLocal)) return true;
  }
  return false;
}
// A WebKitWebProcess that is a direct child of the window process.
async function webviewChildOf(windowPid: number): Promise<boolean> {
  const list = (await sh(["pgrep", "-af", "WebKitWebProcess"])).trim();
  for (const line of list.split("\n")) {
    if (!line) continue;
    const pid = Number(line.split(" ")[0]);
    if (pid === windowPid || Number.isNaN(pid)) continue;
    const status = await sh(["ps", "-o", "ppid=", "-p", String(pid)]);
    if (status.trim() === String(windowPid)) return true;
  }
  return false;
}
try {
  // 1. Isolated core boots and is ready.
  await start();

  // 2. connect_core(): authenticated Status on a fresh command socket.
  {
    const wire = await Wire.connect(await nonce());
    const status = ok(await wire.request(crypto.randomUUID(), { type: "status" })) as { protocol: number; projects: number; last_seq: number; abandoned_requests: number };
    assert.equal(status.protocol, 1, "status protocol");
    assert.ok(status.projects >= 0, "status projects");
    assert.ok(status.last_seq >= 0, "status last_seq");
    wire.close();
  }

  // 3. core_request() typed dispatch: Doctor + ProjectList, correlated by id.
  {
    const wire = await Wire.connect(await nonce());
    const doctor = ok(await wire.request(crypto.randomUUID(), { type: "doctor" })) as { protocol: number; sqlite_integrity: string; foreign_keys: boolean; schema_version: number; journal_mode: string };
    assert.equal(doctor.sqlite_integrity, "ok", "core integrity must be ok on a fresh store");
    assert.ok(doctor.foreign_keys, "foreign_keys must be enforced");
    assert.ok(Array.isArray(ok(await wire.request(crypto.randomUUID(), { type: "project_list" }))), "project_list returns an array");
    wire.close();
  }

  // 4. A project created on a command socket is visible in the list — the read
  //    path the Projects pane renders.
  {
    const wire = await Wire.connect(await nonce());
    const project = ok(await wire.request(crypto.randomUUID(), { type: "project_create", name: "desktop-smoke", root: join(temp, "proj") })) as { id: string; name: string; root: string };
    wire.close();
    const list = await Wire.connect(await nonce());
    const rows = ok(await list.request(crypto.randomUUID(), { type: "project_list" })) as Array<{ id: string }>;
    assert.ok(rows.some((p) => p.id === project.id), "created project missing from list");
    list.close();
  }

  // 5. subscribe_events(): a dedicated subscription socket with durable pull
  //    replay. Seed a state-changing command, subscribe from 0, confirm the
  //    Events pane's feed receives the replayed envelope.
  {
    const seeder = await Wire.connect(await nonce());
    await seeder.request(crypto.randomUUID(), { type: "project_create", name: "desktop-event", root: join(temp, "proj2") });
    seeder.close();
    const sub = await Wire.connect(await nonce());
    sub.subscribe("0");
    const event = await sub.nextEvent();
    assert.ok(BigInt(event.seq) >= 1n, "replayed event must have a real sequence");
    assert.equal(event.protocol, 1, "event protocol");
    assert.equal((event.payload as { type: string }).type, "state_changed", "first durable event is the project creation");
    sub.close();
  }

  // 6. Real-window probe (display required). Launch the actual Tauri binary
  //    against the isolated daemon. The window's webview must come up, and the
  //    daemon and the window must share an ESTAB unix-socket connection —
  //    proof the webview authenticated to the core and opened its sockets.
  if (process.env.DISPLAY || process.env.WAYLAND_DISPLAY) {
    // Debug binary loads the frontend from Vite's devUrl; start the dev server
    // (mirrors `tauri dev`) or the window loads a dead page and never connects.
    await startDevServer();
    window = Bun.spawn([desktopBinary], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
    const windowPid = window.pid;
    const started = Date.now();
    let sawWebview = false;
    let sawSharedSocket = false;
    while (Date.now() - started < 45_000) {
      if (window.exitCode !== null) throw new Error(`window exited early (code ${window.exitCode})`);
      if (!sawWebview) sawWebview = await webviewChildOf(windowPid);
      if (!sawSharedSocket) sawSharedSocket = await sharesUnixConnectionWithDaemon(windowPid);
      if (sawWebview && sawSharedSocket) break;
      await Bun.sleep(500);
    }
    assert.ok(sawWebview, "window webview (WebKitWebProcess child) never came up");
    assert.ok(sawSharedSocket, "daemon never shared an ESTAB unix socket with the window process");
  } else {
    console.log("desktop-connect: no display available; window probe skipped");
  }

  await stop();
  console.log(process.env.DISPLAY || process.env.WAYLAND_DISPLAY
    ? "desktop-connect: native IPC verified + real Tauri window connected (webview child alive, shared ESTAB core.sock connection)"
    : "desktop-connect: native IPC verified (no display; window probe skipped)");
  console.log("PASS");
} finally {
  for (const wire of connections) wire.close();
  await stop();
}
