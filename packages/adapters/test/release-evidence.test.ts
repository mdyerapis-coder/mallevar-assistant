import { expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { releaseCheck, receiptPathFor, type ReleaseManifest } from "../../../qa/qualification/release";
import type { GateReceipt } from "../../../qa/qualification/run-gates";

const runner = resolve(import.meta.dir, "../../../qa/qualification/run-gates.ts");

interface GateFixture {
  root: string;
  gate: string;
  manifest: ReleaseManifest;
  evidence: string;
}

async function fixture(argument = "pass"): Promise<GateFixture> {
  const root = await mkdtemp(join(tmpdir(), "ironbark-gate-evidence-"));
  const gate = `bun gate.ts ${argument}`;
  const manifest: ReleaseManifest = { version: "fixture", components: [], release_gates: { required: [gate] } };
  const files: Record<string, string> = {
    ".gitignore": "evidence/\n.superpowers/\nnode_modules/\ntarget/\n",
    "Cargo.toml": "[workspace]\nmembers = []\n",
    "Cargo.lock": "version = 4\n",
    "rust-toolchain.toml": await Bun.file(resolve(import.meta.dir, "../../../rust-toolchain.toml")).text(),
    "package.json": '{"private":true}',
    "bun.lock": '{"lockfileVersion":1,"workspaces":{"":{"name":"fixture"}},"packages":{}}',
    "packaging/manifest.json": JSON.stringify(manifest),
    "packages/contracts/src/generated.ts": "export type Fixture = string;\n",
    "packages/contracts/schema.json": '{"type":"string"}',
    "src/product.rs": "pub const PRODUCT: u32 = 1;\n",
    "gate.ts": `if (process.argv[2] === 'mutate') await Bun.write('src/product.rs', 'pub const PRODUCT: u32 = 2;\\n');
if (process.argv[2] === 'flood') {
  for (let i = 0; i < 256; i++) { process.stdout.write('o'.repeat(1024)); process.stderr.write('e'.repeat(1024)); }
  process.stdout.write('stdout-end'); process.stderr.write('stderr-end');
}
if (process.argv[2] === 'binary') { process.stdout.write(Buffer.alloc(70000, 255)); process.stderr.write('😀'.repeat(20000) + 'z'); }
if (process.argv[2] === 'nonzero') process.exit(7);
`,
  };
  for (const [path, text] of Object.entries(files)) {
    await mkdir(dirname(join(root, path)), { recursive: true });
    await Bun.write(join(root, path), text);
  }
  for (const args of [["init", "-q"], ["add", "."], ["-c", "user.name=Fixture", "-c", "user.email=fixture@invalid", "commit", "-qm", "fixture"]]) {
    const result = Bun.spawnSync(["git", ...args], { cwd: root, stdout: "pipe", stderr: "pipe" });
    if (result.exitCode !== 0) throw new Error(`fixture git failed: ${result.stderr.toString()}`);
  }
  return { root, gate, manifest, evidence: join(root, "evidence") };
}

async function run(plan: GateFixture, env: NodeJS.ProcessEnv = process.env) {
  // Spawn the runner through the exact executable path: re-resolving the
  // bare "bun" command from inside a bun process mutates the child's PATH
  // (the leading shim entry is dropped), which would make the runner's
  // captured toolchain identity differ from this process's releaseCheck
  // identity and block every gate as "source does not match".
  const child = Bun.spawn([process.execPath, runner, "--manifest", "packaging/manifest.json", "--evidence-dir", "evidence"], {
    cwd: plan.root, env, stdin: "ignore", stdout: "pipe", stderr: "pipe",
  });
  const [exit, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  const path = receiptPathFor(plan.gate, plan.evidence);
  if (!await Bun.file(path).exists()) throw new Error(`runner produced no receipt: ${exit} ${stdout} ${stderr}`);
  const receipt = await Bun.file(path).json() as GateReceipt;
  const report = await releaseCheck(plan.manifest, { root: plan.root, evidenceDir: plan.evidence, manifestPath: "packaging/manifest.json" });
  return { exit, receipt, report };
}

test("changing_a_noncontract_product_file_invalidates_previously_passing_evidence", async () => {
  const plan = await fixture();
  try {
    const before = await run(plan);
    expect(before.exit).toBe(0);
    expect(before.report.state).toBe("passed");
    await Bun.write(join(plan.root, "src/product.rs"), "pub const PRODUCT: u32 = 99;\n");
    const after = await releaseCheck(plan.manifest, { root: plan.root, evidenceDir: plan.evidence, manifestPath: "packaging/manifest.json" });
    expect(after.state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("source_drift_during_an_exit_zero_gate_is_not_a_passing_receipt", async () => {
  const plan = await fixture("mutate");
  try {
    const result = await run(plan);
    expect(result.exit).toBe(1);
    expect(result.receipt.exit_code).toBe(0);
    const before = result.receipt.source_before;
    const after = result.receipt.source_after;
    if (before === null || after === null) throw new Error("drift must be established from two actual source observations");
    expect(before.source_digest).not.toBe(after.source_digest);
    expect(result.receipt.state).toBe("failed");
    expect(result.report.state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test.each(["packages/contracts/schema.json", "packages/contracts/src/generated.ts"])("a_missing_required_input_blocks_proof: %s", async (missing) => {
  const plan = await fixture();
  try {
    expect((await run(plan)).report.state).toBe("passed");
    await rm(join(plan.root, missing));
    const result = await run(plan);
    expect(result.exit).toBe(1);
    expect(result.receipt.exit_code).toBeNull();
    expect(result.receipt.source_before).toBeNull();
    expect(result.receipt.source_after).toBeNull();
    expect(result.report.state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("flooded_stdout_and_stderr_have_independent_bounded_capture_with_loss_counts", async () => {
  const plan = await fixture("flood");
  try {
    const result = await run(plan);
    expect(result.exit).toBe(0);
    expect(result.receipt.stdout.total_bytes).toBe(262154);
    expect(result.receipt.stderr.total_bytes).toBe(262154);
    expect(result.receipt.stdout.dropped_bytes).toBe(196618);
    expect(result.receipt.stderr.dropped_bytes).toBe(196618);
    expect(Buffer.byteLength(result.receipt.stdout.tail)).toBe(65536);
    expect(Buffer.byteLength(result.receipt.stderr.tail)).toBe(65536);
    expect(result.receipt.stdout.tail.endsWith("stdout-end")).toBe(true);
    expect(result.receipt.stderr.tail.endsWith("stderr-end")).toBe(true);
    expect(result.report.state).toBe("passed");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("an_observed_nonzero_exit_remains_a_failed_gate", async () => {
  const plan = await fixture("nonzero");
  try {
    const result = await run(plan);
    expect(result.exit).toBe(1);
    expect(result.receipt.exit_code).toBe(7);
    expect(result.report.state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("a_product_edit_before_the_gate_is_bound_to_the_observed_dirty_source", async () => {
  const plan = await fixture();
  try {
    await Bun.write(join(plan.root, "src/product.rs"), "pub const PRODUCT: u32 = 42;\n");
    const result = await run(plan);
    expect(result.exit).toBe(0);
    expect(result.receipt.source_before?.dirty).toBe(true);
    expect(result.receipt.source_before).toEqual(result.receipt.source_after);
    expect(result.receipt.argv).toEqual(["bun", "gate.ts", "pass"]);
    expect(result.receipt.cwd).toBe(plan.root);
    expect(result.report.state).toBe("passed");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("an_unspawnable_gate_records_failure_without_inventing_an_exit_code", async () => {
  const plan = await fixture();
  try {
    plan.gate = "ironbark-no-such-gate-executable";
    plan.manifest.release_gates = { required: [plan.gate] };
    await Bun.write(join(plan.root, "packaging/manifest.json"), JSON.stringify(plan.manifest));
    const result = await run(plan);
    expect(result.exit).toBe(1);
    expect(result.receipt.exit_code).toBeNull();
    expect(result.receipt.errors.length).toBeGreaterThan(0);
    expect(result.report.state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("quoted_gate_strings_are_rejected_instead_of_approximated_as_shell_argv", async () => {
  const plan = await fixture();
  try {
    plan.gate = 'bun gate.ts "pass"';
    plan.manifest.release_gates = { required: [plan.gate] };
    await Bun.write(join(plan.root, "packaging/manifest.json"), JSON.stringify(plan.manifest));
    const result = await run(plan);
    expect(result.exit).toBe(1);
    expect(result.receipt.argv).toEqual([]);
    expect(result.receipt.exit_code).toBeNull();
    expect(result.report.state).toBe("blocked");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("non_utf8_and_partial_multibyte_tails_preserve_original_byte_counts_losslessly", async () => {
  const plan = await fixture("binary");
  try {
    const result = await run(plan);
    expect(result.exit).toBe(0);
    expect(result.receipt.stdout.encoding).toBe("base64");
    expect(result.receipt.stdout.total_bytes).toBe(70000);
    expect(result.receipt.stdout.dropped_bytes).toBe(4464);
    expect(Buffer.from(result.receipt.stdout.tail, "base64").equals(Buffer.alloc(65536, 255))).toBe(true);
    expect(result.receipt.stderr.encoding).toBe("base64");
    expect(result.receipt.stderr.total_bytes).toBe(80001);
    expect(result.receipt.stderr.dropped_bytes).toBe(14465);
    expect(Buffer.from(result.receipt.stderr.tail, "base64").byteLength).toBe(65536);
    expect(result.report.state).toBe("passed");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("actual_toolchain_executable_changes_invalidate_cli_evidence_even_with_the_same_version", async () => {
  const plan = await fixture();
  try {
    const cargo = Bun.which("cargo");
    if (!cargo) throw new Error("required test toolchain cargo is unavailable");
    const bin = join(plan.evidence, "bin");
    await mkdir(bin, { recursive: true });
    const wrapper = join(bin, "cargo");
    const original = `#!/usr/bin/env bun\nconst child = Bun.spawn([${JSON.stringify(cargo)}, ...process.argv.slice(2)], { stdin: 'ignore', stdout: 'inherit', stderr: 'inherit' });\nprocess.exit(await child.exited);\n`;
    await Bun.write(wrapper, original);
    await chmod(wrapper, 0o755);
    const env = { ...process.env, PATH: `${bin}:${process.env.PATH ?? ""}` };
    const ran = Bun.spawn(["bun", runner, "--manifest", "packaging/manifest.json", "--evidence-dir", "evidence"], { cwd: plan.root, env, stdout: "pipe", stderr: "pipe" });
    const [code] = await Promise.all([ran.exited, new Response(ran.stdout).text(), new Response(ran.stderr).text()]);
    expect(code).toBe(0);
    const checker = resolve(import.meta.dir, "../../../qa/qualification/release.ts");
    for (const expected of [0, 1]) {
      if (expected === 1) await Bun.write(wrapper, `${original}\n// A changed executable with identical observed version output.\n`);
      const child = Bun.spawn(["bun", checker, "--manifest", "packaging/manifest.json", "--evidence-dir", "evidence"], { cwd: plan.root, env, stdout: "pipe", stderr: "pipe" });
      const [exit, stdout] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
      expect(exit).toBe(expected);
      expect(JSON.parse(stdout).state).toBe(expected === 0 ? "passed" : "blocked");
    }
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test("undeclared_parent_environment_cannot_influence_the_gate_child", async () => {
  const plan = await fixture();
  try {
    await Bun.write(join(plan.root, "gate.ts"), "console.log(process.env.IRONBARK_UNDECLARED_GATE_FLAG ?? 'absent');\n");
    const result = await run(plan, { ...process.env, IRONBARK_UNDECLARED_GATE_FLAG: "private-test-canary" });
    expect(result.exit).toBe(0);
    expect(result.receipt.stdout.tail).toBe("absent\n");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});

test.each([".env", ".env.local", ".env.test", ".env.development"])("private_automatic_environment_inputs_block_managed_gates_without_disclosure: %s", async (path) => {
  const plan = await fixture();
  try {
    expect((await run(plan)).report.state).toBe("passed");
    await Bun.write(join(plan.root, ".gitignore"), `evidence/\n.superpowers/\n${path}\n`);
    await Bun.write(join(plan.root, path), "IRONBARK_UNDECLARED_GATE_FLAG=private-dotenv-canary\n");
    await Bun.write(join(plan.root, "gate.ts"), "console.log(process.env.IRONBARK_UNDECLARED_GATE_FLAG ?? 'absent');\n");
    const result = await run(plan);
    expect(result.exit).toBe(1);
    expect(result.receipt.exit_code).toBeNull();
    expect(result.receipt.source_before).toBeNull();
    expect(result.receipt.stdout.total_bytes).toBe(0);
    expect(result.report.state).toBe("blocked");
    expect(JSON.stringify(result.receipt)).not.toContain("private-dotenv-canary");
  } finally { await rm(plan.root, { recursive: true, force: true }); }
});
