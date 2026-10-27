#!/usr/bin/env bun
/** Execute required gates without a shell; write exact-source v2 receipts.
 * Exit 0: every selected gate observed passing, 1: failed/blocked, 2: usage.
 * Output capture is a fixed 64 KiB byte ring per channel, drained concurrently.
 */
import { mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import { isAbsolute, relative, resolve } from "node:path";
import { defaultEvidenceDir, gateSlug, receiptPathFor, type ReleaseManifest } from "./release";
import { argvForGate, captureSourceIdentity, gateEnvironment, sameSourceIdentity, type SourceIdentity } from "./source-identity";

export interface ChannelReceipt {
  tail: string;
  total_bytes: number;
  dropped_bytes: number;
  encoding: "utf8" | "base64";
}
export interface GateReceipt {
  schema_version: 2;
  gate: string;
  state: "passed" | "failed";
  command: string;
  argv: string[];
  cwd: string;
  manifest_path: string;
  manifest_sha256: string | null;
  recorded_at: string;
  started_at: string;
  finished_at: string;
  exit_code: number | null;
  stdout: ChannelReceipt;
  stderr: ChannelReceipt;
  source_before: SourceIdentity | null;
  source_after: SourceIdentity | null;
  errors: string[];
}

const CHANNEL_BYTES = 64 * 1024;
async function captureChannel(stream: ReadableStream<Uint8Array>): Promise<ChannelReceipt> {
  const ring = Buffer.alloc(CHANNEL_BYTES);
  let total = 0;
  let cursor = 0;
  const reader = stream.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.byteLength;
      if (value.byteLength >= CHANNEL_BYTES) {
        ring.set(value.subarray(value.byteLength - CHANNEL_BYTES));
        cursor = 0;
      } else {
        const first = Math.min(value.byteLength, CHANNEL_BYTES - cursor);
        ring.set(value.subarray(0, first), cursor);
        ring.set(value.subarray(first), 0);
        cursor = (cursor + value.byteLength) % CHANNEL_BYTES;
      }
    }
  } finally {
    reader.releaseLock();
  }
  const retained = Math.min(total, CHANNEL_BYTES);
  const bytes = total < CHANNEL_BYTES ? ring.subarray(0, retained) : Buffer.concat([ring.subarray(cursor), ring.subarray(0, cursor)], retained);
  try {
    // Binary output or a UTF-8 sequence cut at the byte boundary stays lossless
    // in base64; counters always describe original bytes, not JS characters.
    return { tail: new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes), encoding: "utf8", total_bytes: total, dropped_bytes: total - retained };
  } catch {
    return { tail: bytes.toString("base64"), encoding: "base64", total_bytes: total, dropped_bytes: total - retained };
  }
}

function display(root: string, path: string): string {
  const rel = relative(root, path);
  return rel === "" || rel.startsWith("..") || isAbsolute(rel) ? path : rel;
}

/** Failed identity capture never launches the gate. A changed post-identity
 * fails even for exit 0. Errors are separate from observed process channels. */
export async function runGate(gate: string, root: string, evidenceDir: string, manifestPath = "packaging/manifest.json"): Promise<{ receipt: GateReceipt; path: string }> {
  root = await realpath(root);
  const started = new Date();
  const environment = gateEnvironment();
  let argv: string[] = [];
  let before: SourceIdentity | null = null;
  let after: SourceIdentity | null = null;
  let exitCode: number | null = null;
  let stdout: ChannelReceipt = { tail: "", total_bytes: 0, dropped_bytes: 0, encoding: "utf8" };
  let stderr: ChannelReceipt = { ...stdout };
  const manifestRelative = relative(root, resolve(root, manifestPath));
  let manifestDigest: string | null = null;
  const errors: string[] = [];
  try {
    argv = argvForGate(gate);
    before = await captureSourceIdentity(root, environment);
    const selectedManifest = before.files.find((file) => file.path === manifestRelative && file.mode !== "120000");
    if (!selectedManifest) throw new Error("gate manifest is not a selected regular source input; add its repository-relative path to source_inventory.required");
    manifestDigest = selectedManifest.sha256;
    const proc = Bun.spawn(argv, { cwd: root, env: environment, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
    // Start both bounded drains before waiting for the child. allSettled keeps
    // draining the other channel even if one capture fails.
    const results = await Promise.allSettled([captureChannel(proc.stdout), captureChannel(proc.stderr), proc.exited]);
    if (results[0].status === "fulfilled") stdout = results[0].value;
    if (results[1].status === "fulfilled") stderr = results[1].value;
    if (results[2].status === "fulfilled") exitCode = results[2].value;
    for (const result of results) if (result.status === "rejected") errors.push(`gate observation failed: ${String(result.reason)}; rerun with working process pipes`);
    if (exitCode !== 0) errors.push(`gate exited ${exitCode ?? "without an observed status"}; fix the gate failure and rerun`);
  } catch (error) {
    errors.push(`${error instanceof Error ? error.message : String(error)}; fix the command or required inputs and rerun`);
  }
  try {
    after = await captureSourceIdentity(root, environment);
    if (before && !sameSourceIdentity(before, after)) errors.push("source changed during gate; stop product mutations and rerun against the final joined source");
  } catch (error) {
    errors.push(`post-gate identity failed: ${error instanceof Error ? error.message : String(error)}; restore required inputs and rerun`);
  }
  const finished = new Date().toISOString();
  const receipt: GateReceipt = {
    schema_version: 2, gate, state: exitCode === 0 && before !== null && after !== null && errors.length === 0 ? "passed" : "failed",
    command: argv.join(" "), argv, cwd: root,
    manifest_path: manifestRelative, manifest_sha256: manifestDigest,
    recorded_at: finished, started_at: started.toISOString(), finished_at: finished,
    exit_code: exitCode, stdout, stderr, source_before: before, source_after: after, errors,
  };
  const path = receiptPathFor(gate, evidenceDir);
  await mkdir(evidenceDir, { recursive: true });
  await writeFile(path, `${JSON.stringify(receipt, null, 2)}\n`);
  return { receipt, path };
}

function usage(): never {
  console.error(
    "usage: bun qa/qualification/run-gates.ts [--manifest <path>] [--evidence-dir <dir>] [--only <gate>] [--dry-run]",
  );
  process.exit(2);
}

async function main(): Promise<void> {
  const argv = process.argv.slice(2);
  let manifestArg: string | undefined;
  let evidenceArg: string | undefined;
  let only: string | undefined;
  let dryRun = false;
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--dry-run") {
      dryRun = true;
      continue;
    }
    if (flag !== "--manifest" && flag !== "--evidence-dir" && flag !== "--only") usage();
    const next: string | undefined = argv[index + 1];
    if (next === undefined || next.startsWith("--")) usage();
    if (flag === "--manifest") manifestArg = next;
    else if (flag === "--evidence-dir") evidenceArg = next;
    else only = next;
    index += 1; // each value flag consumes its value
  }

  const root = await realpath(process.cwd());
  const manifestPath = resolve(root, manifestArg ?? "packaging/manifest.json");
  let manifest: ReleaseManifest;
  try {
    manifest = JSON.parse(await readFile(manifestPath, "utf8")) as ReleaseManifest;
  } catch (error) {
    console.error(
      `run-gates: manifest ${display(root, manifestPath)} cannot be read: ${error instanceof Error ? error.message : String(error)}`,
    );
    process.exit(1);
  }

  const required: unknown = manifest.release_gates?.required;
  if (!Array.isArray(required) || required.length === 0 || required.some((gate) => typeof gate !== "string" || !gate.trim())) {
    console.error(`run-gates: manifest ${display(root, manifestPath)} needs a nonempty release_gates.required string array; restore the required gate list`);
    process.exit(1);
  }
  const gates = required as string[];
  const selected = only === undefined ? gates : gates.filter((gate) => gate === only);
  if (only !== undefined && selected.length === 0) {
    console.error(`run-gates: --only ${JSON.stringify(only)} does not name a required gate of ${display(root, manifestPath)}`);
    process.exit(2);
  }

  const evidenceDir = evidenceArg ? resolve(root, evidenceArg) : await defaultEvidenceDir(root);

  if (dryRun) {
    const plan = {
      dry_run: true,
      manifest: display(root, manifestPath),
      evidence_dir: display(root, evidenceDir),
      gates: selected.map((gate) => ({
        gate,
        slug: gateSlug(gate),
        argv: argvForGate(gate),
        cwd: root,
        command: argvForGate(gate).join(" "),
        receipt: display(root, receiptPathFor(gate, evidenceDir)),
      })),
    };
    console.log(JSON.stringify(plan, null, 2));
    process.exit(0);
  }

  const results: { gate: string; state: string; command: string; receipt: string; exit_code: number | null }[] = [];
  for (const gate of selected) {
    const { receipt, path } = await runGate(gate, root, evidenceDir, manifestPath);
    results.push({
      gate: receipt.gate,
      state: receipt.state,
      command: receipt.command,
      receipt: display(root, path),
      exit_code: receipt.exit_code,
    });
  }
  const passed = results.filter((result) => result.state === "passed").length;
  console.log(
    JSON.stringify(
      {
        state: passed === results.length ? "passed" : "failed",
        manifest: display(root, manifestPath),
        evidence_dir: display(root, evidenceDir),
        gates: results,
      },
      null,
      2,
    ),
  );
  console.error(`release gates: ${passed}/${results.length} passed`);
  process.exit(passed === results.length ? 0 : 1);
}

if (import.meta.main) {
  try {
    await main();
  } catch (error) {
    console.log(JSON.stringify({ state: "failed", error: `${error instanceof Error ? error.message : String(error)}; correct the gate plan or evidence path and rerun` }));
    process.exit(1);
  }
}
