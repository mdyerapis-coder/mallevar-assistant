#!/usr/bin/env bun
import { expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { releaseCheck, receiptPathFor, verifyGateReceipt, type ReleaseManifest } from "./release";
import { runGate, type GateReceipt } from "./run-gates";
import { captureSourceIdentity } from "./source-identity";

interface Fixture { root: string; evidence: string; gate: string; manifest: ReleaseManifest }
async function fixture(): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), "ib-release-checker-"));
  const gate = "bun gate.ts";
  const manifest: ReleaseManifest = { version: "fixture", components: [], release_gates: { required: [gate] } };
  const files: Record<string, string> = {
    ".gitignore": "evidence/\n.superpowers/\nignored-input/\n",
    "packaging/manifest.json": JSON.stringify(manifest),
    "Cargo.toml": "[workspace]\nmembers=[]\n", "Cargo.lock": "version = 4\n",
    "rust-toolchain.toml": await Bun.file(resolve(import.meta.dir, "../../rust-toolchain.toml")).text(),
    "package.json": '{"private":true}', "bun.lock": '{}',
    "packages/contracts/src/generated.ts": "export type Fixture = string;\n",
    "packages/contracts/schema.json": '{"type":"string"}',
    "src/product.rs": "pub const VALUE: u32 = 1;\n", "gate.ts": "console.log('observed-gate');\n",
  };
  for (const [path, text] of Object.entries(files)) {
    await mkdir(dirname(join(root, path)), { recursive: true });
    await Bun.write(join(root, path), text);
  }
  for (const args of [["init", "-q"], ["add", "."], ["-c", "user.name=Fixture", "-c", "user.email=fixture@invalid", "commit", "-qm", "fixture"]]) {
    const git = Bun.spawnSync(["git", ...args], { cwd: root });
    if (git.exitCode !== 0) throw new Error(git.stderr.toString());
  }
  return { root, gate, manifest, evidence: join(root, "evidence") };
}
async function check(plan: Fixture) {
  return releaseCheck(plan.manifest, { root: plan.root, evidenceDir: plan.evidence, manifestPath: "packaging/manifest.json" });
}
async function save(plan: Fixture, receipt: GateReceipt) {
  await Bun.write(receiptPathFor(plan.gate, plan.evidence), JSON.stringify(receipt));
}

test("checker accepts actual observations and blocks missing, malformed and legacy-only receipts", async () => {
  const plan = await fixture();
  try {
    expect((await check(plan)).state).toBe("blocked");
    await runGate(plan.gate, plan.root, plan.evidence);
    expect((await check(plan)).state).toBe("passed");
    await Bun.write(receiptPathFor(plan.gate, plan.evidence), "not json");
    expect((await check(plan)).state).toBe("blocked");
    await Bun.write(receiptPathFor(plan.gate, plan.evidence), JSON.stringify({ gate: plan.gate, command: plan.gate, state: "passed" }));
    expect((await check(plan)).state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("checker rejects altered argv, cwd, toolchain and exit independently of a passed label", async () => {
  const plan = await fixture();
  try {
    const { receipt } = await runGate(plan.gate, plan.root, plan.evidence);
    const identity = await captureSourceIdentity(plan.root);
    expect(verifyGateReceipt(receipt, identity).outcome).toBe("passed");
    for (const mutate of [
      (r: GateReceipt) => { r.argv.push("not-observed"); },
      (r: GateReceipt) => { r.cwd = dirname(plan.root); },
      (r: GateReceipt) => { if (r.source_before) r.source_before.toolchain.bun = "not-the-observed-toolchain"; },
      (r: GateReceipt) => { if (r.source_after) r.source_after.environment.SOURCE_DATE_EPOCH = "123"; },
      (r: GateReceipt) => { r.manifest_path = "another-manifest.json"; },
      (r: GateReceipt) => { r.manifest_sha256 = "0".repeat(64); },
      (r: GateReceipt) => { r.exit_code = 7; },
      (r: GateReceipt) => { r.stdout.dropped_bytes += 1; },
    ]) {
      const changed = structuredClone(receipt);
      mutate(changed);
      await save(plan, changed);
      expect((await check(plan)).state).toBe("blocked");
    }
    const changedToolchain = structuredClone(identity);
    changedToolchain.toolchain.bun = "a-different-current-toolchain";
    expect(verifyGateReceipt(receipt, changedToolchain).reasons).toContain("source_before does not match required source identity");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("untracked selected product inputs and file modes invalidate older observations, evidence does not", async () => {
  const plan = await fixture();
  try {
    await runGate(plan.gate, plan.root, plan.evidence);
    await Bun.write(join(plan.evidence, "notes.json"), "{}");
    expect((await check(plan)).state).toBe("passed");
    await Bun.write(join(plan.root, "src/new.rs"), "pub const NEW: bool = true;\n");
    expect((await check(plan)).state).toBe("blocked");
    await runGate(plan.gate, plan.root, plan.evidence);
    expect((await check(plan)).state).toBe("passed");
    await chmod(join(plan.root, "src/product.rs"), 0o755);
    expect((await check(plan)).state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("explicit ignored required inputs are hashed and their deletion blocks capture", async () => {
  const plan = await fixture();
  try {
    plan.manifest.source_inventory = { required: ["ignored-input/config.json"], untracked_roots: ["src/"] };
    await Bun.write(join(plan.root, "packaging/manifest.json"), JSON.stringify(plan.manifest));
    await mkdir(join(plan.root, "ignored-input"));
    await Bun.write(join(plan.root, "ignored-input/config.json"), "{}");
    await runGate(plan.gate, plan.root, plan.evidence);
    expect((await check(plan)).state).toBe("passed");
    await Bun.write(join(plan.root, "ignored-input/config.json"), '{"changed":true}');
    expect((await check(plan)).state).toBe("blocked");
    await rm(join(plan.root, "ignored-input/config.json"));
    await expect(captureSourceIdentity(plan.root)).rejects.toThrow();
    expect((await check(plan)).state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("selected symlink escape and missing locks fail closed", async () => {
  const plan = await fixture();
  try {
    await symlink(tmpdir(), join(plan.root, "src/escape"));
    await expect(captureSourceIdentity(plan.root)).rejects.toThrow();
    await rm(join(plan.root, "src/escape"));
    await rm(join(plan.root, "Cargo.lock"));
    const result = await runGate(plan.gate, plan.root, plan.evidence);
    expect(result.receipt.state).toBe("failed");
    expect(result.receipt.exit_code).toBeNull();
    expect((await check(plan)).state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("release CLI checks current disposable checkout and reports usage distinctly", async () => {
  const plan = await fixture();
  try {
    await runGate(plan.gate, plan.root, plan.evidence);
    const cli = resolve(import.meta.dir, "release.ts");
    for (const expected of [0, 1]) {
      if (expected === 1) await Bun.write(join(plan.root, "src/product.rs"), "pub const VALUE: u32 = 9;\n");
      const child = Bun.spawn([process.execPath, cli, "--manifest", "packaging/manifest.json", "--evidence-dir", "evidence"], { cwd: plan.root, stdout: "pipe", stderr: "pipe" });
      const [code, output] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
      expect(code).toBe(expected);
      expect(JSON.parse(output).state).toBe(expected === 0 ? "passed" : "blocked");
    }
    expect(Bun.spawnSync([process.execPath, cli, "--unknown"], { cwd: plan.root }).exitCode).toBe(2);
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("a_new_commit_invalidates_receipts_even_when_selected_file_bytes_are_identical", async () => {
  const plan = await fixture();
  try {
    const { receipt } = await runGate(plan.gate, plan.root, plan.evidence);
    const before = receipt.source_before;
    if (before === null) throw new Error("passing gate must have an observed source identity");
    expect((await check(plan)).state).toBe("passed");
    const committed = Bun.spawnSync(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@invalid", "commit", "--allow-empty", "-qm", "different commit, same tree"], { cwd: plan.root });
    expect(committed.exitCode).toBe(0);
    const current = await captureSourceIdentity(plan.root);
    expect(current.tree).toBe(before.tree);
    expect(current.source_digest).toBe(before.source_digest);
    expect(current.commit).not.toBe(before.commit);
    expect((await check(plan)).state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("changing_the_effective_daemon_selector_invalidates_existing_gate_evidence", async () => {
  const plan = await fixture();
  try {
    await runGate(plan.gate, plan.root, plan.evidence);
    expect((await check(plan)).state).toBe("passed");
    const child = Bun.spawn([process.execPath, resolve(import.meta.dir, "release.ts"), "--manifest", "packaging/manifest.json", "--evidence-dir", "evidence"], {
      cwd: plan.root, env: { ...process.env, IRONBARK_CORE_BIN: join(plan.root, "other-core") }, stdout: "pipe", stderr: "pipe",
    });
    const [code, output] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
    expect(code).toBe(1);
    expect(JSON.parse(output).state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test.each([
  [".cargo/config.toml", "[build]\nincremental = false\n", "[build]\nincremental = true\n"],
  ["bunfig.toml", "[test]\ntimeout = 5000\n", "[test]\ntimeout = 4000\n"],
])("automatic_repository_configuration_cannot_bypass_gate_identity: %s", async (path, first, second) => {
  const plan = await fixture();
  try {
    await runGate(plan.gate, plan.root, plan.evidence);
    expect((await check(plan)).state).toBe("passed");
    await mkdir(dirname(join(plan.root, path)), { recursive: true });
    await Bun.write(join(plan.root, path), first);
    expect((await check(plan)).state).toBe("blocked");
    await runGate(plan.gate, plan.root, plan.evidence);
    expect((await check(plan)).state).toBe("passed");
    await Bun.write(join(plan.root, path), second);
    expect((await check(plan)).state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});
