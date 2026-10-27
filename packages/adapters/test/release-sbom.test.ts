import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { captureSourceIdentity, type SourceIdentity } from "../../../qa/qualification/source-identity";
// Deliberately no fallback: the parent must observe RED before implementation.
import { generateSbom } from "../../../qa/qualification/sbom";

type Material = Parameters<typeof generateSbom>[1][number];
interface Component {
  "bom-ref"?: string; name: string; version?: string; group?: string;
  purl?: string; description?: string; author?: string; supplier?: { name?: string };
  hashes?: { alg: string; content: string }[];
  licenses?: { license?: { id?: string; name?: string }; expression?: string }[];
  externalReferences?: { type: string; url: string }[];
  properties?: { name: string; value: string }[];
  components?: Component[];
}
interface Bom {
  bomFormat: string; specVersion: string; version: number;
  metadata?: { component?: Component };
  components?: Component[];
  dependencies?: { ref: string; dependsOn?: string[] }[];
  annotations?: { subjects: string[]; text: string }[];
}
const sha256 = (bytes: string) => createHash("sha256").update(bytes).digest("hex");
const sri = (bytes: string) => `sha512-${createHash("sha512").update(bytes).digest("base64")}`;
const registry = "registry+https://github.com/rust-lang/crates.io-index";
const alternate = "registry+https://registry.example.invalid/index";
const cargoBlocks = [
  `[[package]]\nname = "native-root"\nversion = "0.1.0"\ndependencies = ["native-direct", "twin 1.0.0 (${registry})", "twin 2.0.0", "twin 1.0.0 (${alternate})"]`,
  `[[package]]\nname = "native-direct"\nversion = "3.0.0"\nsource = "${registry}"\nchecksum = "${"a".repeat(64)}"\ndependencies = ["native-leaf"]`,
  `[[package]]\nname = "native-leaf"\nversion = "4.0.0"\nsource = "${registry}"\nchecksum = "${"b".repeat(64)}"`,
  `[[package]]\nname = "twin"\nversion = "1.0.0"\nsource = "${registry}"\nchecksum = "${"c".repeat(64)}"`,
  `[[package]]\nname = "twin"\nversion = "2.0.0"\nsource = "${registry}"\nchecksum = "${"d".repeat(64)}"`,
  `[[package]]\nname = "twin"\nversion = "1.0.0"\nsource = "${alternate}"\nchecksum = "${"e".repeat(64)}"`,
];
const cargoLock = (blocks = cargoBlocks) => `version = 4\n\n${blocks.join("\n\n")}\n`;
function bunValue() {
  return {
    lockfileVersion: 2, configVersion: 1,
    workspaces: {
      "": { name: "web-root", version: "0.1.0", dependencies: { renderer: "2.0.0", bridge: "3.0.0", "@fixture/workspace": "workspace:*" } },
      "packages/workspace": { name: "@fixture/workspace", version: "0.5.0", dependencies: { renderer: "2.0.0" } },
    },
    packages: {
      "@fixture/workspace": ["@fixture/workspace@workspace:packages/workspace"],
      renderer: ["renderer@2.0.0", "", { dependencies: { "js-leaf": "^1.0.0" }, optionalDependencies: { image: "1.0.0" } }, sri("renderer archive")],
      bridge: ["bridge@3.0.0", "", { dependencies: { "js-leaf": "^2.0.0" } }, sri("bridge archive")],
      "js-leaf": ["js-leaf@1.2.0", "", {}, sri("leaf one archive")],
      "bridge/js-leaf": ["js-leaf@2.3.0", "", {}, sri("leaf two archive")],
      image: ["image@1.0.0", "", { os: "darwin", cpu: "arm64" }, sri("optional image archive")],
    },
  };
}
// Real JSONC: line/block comments, trailing commas, and URL punctuation in strings.
function bunJsonc() {
  const value = bunValue();
  return `/* selected lock; https://example.invalid/not-a-comment */\n${JSON.stringify(value, null, 2).replace(/\n}$/, ",\n}")}\n// end of selected lock\n`;
}
interface Fixture { root: string; source: SourceIdentity; materials: Material[] }
async function fixture(options: { cargo?: string; bun?: string } = {}): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), "ironbark-sbom-test-"));
  try {
    const files: Record<string, string> = {
      ".gitignore": "evidence/\nnode_modules/\ntarget/\n",
      "Cargo.toml": '[package]\nname="native-root"\nversion="0.1.0"\nedition="2021"\n',
      "Cargo.lock": options.cargo ?? cargoLock(),
      "src/lib.rs": "pub const FIXTURE: bool = true;\n",
      "rust-toolchain.toml": await Bun.file(resolve(import.meta.dir, "../../../rust-toolchain.toml")).text(),
      "package.json": JSON.stringify({ private: true, name: "web-root", version: "0.1.0", workspaces: ["packages/workspace"], dependencies: bunValue().workspaces[""].dependencies }),
      "packages/workspace/package.json": JSON.stringify(bunValue().workspaces["packages/workspace"]),
      "bun.lock": options.bun ?? bunJsonc(),
      "packaging/manifest.json": JSON.stringify({ version: "sbom-fixture", components: [], release_gates: { required: [] } }),
      "packages/contracts/src/generated.ts": "export type Fixture = string;\n",
      "packages/contracts/schema.json": '{"type":"string"}',
      "integrations/engine-v1.bin": "owned integration version one\n",
      "integrations/engine-v2.bin": "owned integration version two\n",
    };
    for (const [path, text] of Object.entries(files)) {
      await mkdir(dirname(join(root, path)), { recursive: true });
      await Bun.write(join(root, path), text);
    }
    for (const args of [["init", "-q"], ["add", "."], ["-c", "user.name=Fixture", "-c", "user.email=fixture@invalid", "commit", "-qm", "SBOM fixture"]]) {
      const result = Bun.spawnSync(["git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false", ...args], { cwd: root, env: { PATH: process.env.PATH ?? "/usr/bin:/bin", HOME: process.env.HOME ?? root, GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z", GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z" }, stdout: "pipe", stderr: "pipe" });
      if (result.exitCode !== 0) throw new Error(`fixture git failed: ${result.stderr.toString()}`);
    }
    const source = await captureSourceIdentity(root);
    const materials: Material[] = [
      { name: "engine", version: "1.0.0", ecosystem: "generic", source: "integrations/engine-v1.bin", sha256: sha256(files["integrations/engine-v1.bin"]), purl: "pkg:generic/engine@1.0.0", license: "MIT" },
      { name: "engine", version: "2.0.0", ecosystem: "generic", source: "integrations/engine-v2.bin", sha256: sha256(files["integrations/engine-v2.bin"]), purl: "pkg:generic/engine@2.0.0" },
    ];
    return { root, source, materials };
  } catch (error) {
    await rm(root, { recursive: true, force: true });
    throw error;
  }
}
async function withFixture(run: (fixture: Fixture) => Promise<void>, options?: Parameters<typeof fixture>[0]) {
  const owned = await fixture(options);
  try { await run(owned); } finally { await rm(owned.root, { recursive: true, force: true }); }
}
function components(bom: Bom): Component[] {
  const flatten = (items: Component[]): Component[] => items.flatMap((item) => [item, ...flatten(item.components ?? [])]);
  return flatten([...(bom.metadata?.component ? [bom.metadata.component] : []), ...(bom.components ?? [])]);
}
function one(bom: Bom, name: string, version: string, checksum?: string): Component {
  const matches = components(bom).filter((item) => (item.group ? `${item.group}/${item.name}` : item.name) === name && item.version === version && (checksum === undefined || item.hashes?.some((hash) => hash.content === checksum)));
  expect(matches).toHaveLength(1);
  const found = matches[0];
  if (!found || !found["bom-ref"]) throw new Error(`missing unambiguous component ref: ${name}@${version}`);
  return found;
}
function ref(component: Component): string {
  if (!component["bom-ref"]) throw new Error(`missing bom-ref: ${component.name}`);
  return component["bom-ref"];
}
function edges(bom: Bom, component: Component): string[] {
  const entries = (bom.dependencies ?? []).filter((edge) => edge.ref === ref(component));
  expect(entries).toHaveLength(1);
  return [...(entries[0]?.dependsOn ?? [])].sort();
}
function validGraph(bom: Bom) {
  expect(bom.bomFormat).toBe("CycloneDX");
  expect(bom.specVersion).toBe("1.6");
  expect(Number.isInteger(bom.version) && bom.version >= 1).toBe(true);
  const refs = components(bom).map(ref);
  expect(new Set(refs).size).toBe(refs.length);
  const dependencyRefs = (bom.dependencies ?? []).map((edge) => edge.ref);
  expect(new Set(dependencyRefs).size).toBe(dependencyRefs.length);
  for (const edge of bom.dependencies ?? []) {
    expect(refs).toContain(edge.ref);
    expect(new Set(edge.dependsOn ?? []).size).toBe((edge.dependsOn ?? []).length);
    for (const target of edge.dependsOn ?? []) expect(refs).toContain(target);
  }
}

test("SBOM includes the full Cargo graph, not only direct dependencies or a flat package list", async () => {
  await withFixture(async ({ source, materials }) => {
    const bom: Bom = await generateSbom(source, materials);
    validGraph(bom);
    const direct = one(bom, "native-direct", "3.0.0");
    const leaf = one(bom, "native-leaf", "4.0.0");
    const twin1 = one(bom, "twin", "1.0.0", "c".repeat(64));
    const twin2 = one(bom, "twin", "2.0.0", "d".repeat(64));
    const otherSource = one(bom, "twin", "1.0.0", "e".repeat(64));
    const nativeRoot = one(bom, "native-root", "0.1.0");
    const nativeRefs = [nativeRoot, direct, leaf, twin1, twin2, otherSource].map(ref);
    // The release metadata component may also be an ecosystem root; compare
    // its exact ecosystem graph without prescribing a synthetic release node.
    expect(edges(bom, nativeRoot).filter((target) => nativeRefs.includes(target))).toEqual([direct, twin1, twin2, otherSource].map(ref).sort());
    expect(edges(bom, direct)).toEqual([ref(leaf)]);
    for (const node of [leaf, twin1, twin2, otherSource]) expect(edges(bom, node)).toEqual([]);
    expect(direct.hashes).toContainEqual({ alg: "SHA-256", content: "a".repeat(64) });
    expect(leaf.hashes).toContainEqual({ alg: "SHA-256", content: "b".repeat(64) });
    expect(new Set([twin1, twin2, otherSource].map(ref)).size).toBe(3);
    // A source-qualified identity may use purl qualifiers or standard references.
    const origin = JSON.stringify({ purl: otherSource.purl, references: otherSource.externalReferences });
    expect(origin).toContain("registry.example.invalid");
  });
});

test("JSONC Bun graph resolves nested versions, workspace links, and pinned optional platforms", async () => {
  await withFixture(async ({ source, materials }) => {
    const bom: Bom = await generateSbom(source, materials);
    validGraph(bom);
    const renderer = one(bom, "renderer", "2.0.0");
    const bridge = one(bom, "bridge", "3.0.0");
    const leaf1 = one(bom, "js-leaf", "1.2.0");
    const leaf2 = one(bom, "js-leaf", "2.3.0");
    const optional = one(bom, "image", "1.0.0");
    const workspace = one(bom, "@fixture/workspace", "0.5.0");
    const webRoot = one(bom, "web-root", "0.1.0");
    const webRefs = [webRoot, renderer, bridge, leaf1, leaf2, optional, workspace].map(ref);
    expect(edges(bom, webRoot).filter((target) => webRefs.includes(target))).toEqual([renderer, bridge, workspace].map(ref).sort());
    expect(edges(bom, renderer)).toEqual([leaf1, optional].map(ref).sort());
    expect(edges(bom, bridge)).toEqual([ref(leaf2)]);
    expect(edges(bom, workspace)).toEqual([ref(renderer)]);
    expect(ref(leaf1)).not.toBe(ref(leaf2));
    for (const node of [leaf1, leaf2, optional]) expect(edges(bom, node)).toEqual([]);
    expect(renderer.purl).toBe("pkg:npm/renderer@2.0.0");
    expect(renderer.hashes).toContainEqual({ alg: "SHA-512", content: createHash("sha512").update("renderer archive").digest("hex") });
  });
});

test("shipped materials retain separate versions, observed hashes, purls, license and release reachability", async () => {
  await withFixture(async ({ source, materials }) => {
    const bom: Bom = await generateSbom(source, materials);
    validGraph(bom);
    const first = one(bom, "engine", "1.0.0");
    const second = one(bom, "engine", "2.0.0");
    expect(ref(first)).not.toBe(ref(second));
    for (const [index, component] of [first, second].entries()) {
      expect(component.hashes).toContainEqual({ alg: "SHA-256", content: materials[index]?.sha256 });
      expect(component.purl).toBe(materials[index]?.purl);
    }
    expect(first.licenses?.some((entry) => entry.license?.id === "MIT" || entry.expression === "MIT")).toBe(true);
    const root = bom.metadata?.component;
    if (!root) throw new Error("release metadata component is required for material reachability");
    const reachable = new Set<string>();
    const visit = (current: string) => {
      if (reachable.has(current)) return;
      reachable.add(current);
      for (const next of bom.dependencies?.find((edge) => edge.ref === current)?.dependsOn ?? []) visit(next);
    };
    visit(ref(root));
    expect(reachable.has(ref(first))).toBe(true);
    expect(reachable.has(ref(second))).toBe(true);
  });
});

test("unavailable license and supplier metadata is explicit, not guessed from the ecosystem", async () => {
  await withFixture(async ({ source, materials }) => {
    const bom: Bom = await generateSbom(source, materials);
    for (const component of [one(bom, "native-leaf", "4.0.0"), one(bom, "js-leaf", "1.2.0"), one(bom, "engine", "2.0.0")]) {
      if (component.author !== undefined) expect(component.author).toMatch(/unknown|NOASSERTION/i);
      if (component.supplier !== undefined) expect(component.supplier.name).toMatch(/unknown|NOASSERTION/i);
      expect(component.licenses?.some((entry) => entry.license?.id || (entry.expression && !/^(NOASSERTION|NONE)$/i.test(entry.expression)))).not.toBe(true);
      // Standard free-text fields/properties or subject-bound annotations may
      // express uncertainty; no Ironbark-specific property key is prescribed.
      const statements = [component.description ?? "", ...(component.properties ?? []).map((entry) => `${entry.name}: ${entry.value}`), ...(component.licenses ?? []).map((entry) => entry.license?.name ?? entry.expression ?? ""), ...(bom.annotations ?? []).filter((entry) => entry.subjects.includes(ref(component))).map((entry) => entry.text)];
      expect(statements.some((text) => /unknown|unavailable|not supplied|not asserted|NOASSERTION/i.test(text))).toBe(true);
    }
  });
});

for (const path of ["Cargo.lock", "bun.lock"]) {
  test(`selected ${path} changed after capture is refused, even for comment-only drift`, async () => {
    await withFixture(async ({ root, source, materials }) => {
      const original = await Bun.file(join(root, path)).text();
      await Bun.write(join(root, path), `${original}\n${path === "Cargo.lock" ? "#" : "//"} changed after capture\n`);
      await expect(generateSbom(source, materials)).rejects.toThrow();
    });
  });
  test(`missing ${path} inventory binding is not silently read from the working tree`, async () => {
    await withFixture(async ({ source, materials }) => {
      const files = source.files.filter((file) => file.path !== path);
      const unbound: SourceIdentity = { ...source, files, source_digest: sha256(JSON.stringify(files)) };
      await expect(generateSbom(unbound, materials)).rejects.toThrow();
    });
  });
}

test("a false material digest is refused rather than copied into the BOM", async () => {
  await withFixture(async ({ source, materials }) => {
    const first = materials[0];
    if (!first) throw new Error("missing fixture material");
    await expect(generateSbom(source, [{ ...first, sha256: "0".repeat(64) }])).rejects.toThrow();
  });
});
test("material bytes changed after source capture are refused despite a matching claimed new digest", async () => {
  await withFixture(async ({ root, source, materials }) => {
    const first = materials[0];
    if (!first) throw new Error("missing fixture material");
    const changed = "replacement integration bytes\n";
    await Bun.write(join(root, first.source), changed);
    await expect(generateSbom(source, [{ ...first, sha256: sha256(changed) }])).rejects.toThrow();
  });
});
test("missing observed material cannot be replaced by its claimed identity", async () => {
  await withFixture(async ({ root, source, materials }) => {
    await rm(join(root, "integrations/engine-v1.bin"));
    await expect(generateSbom(source, materials)).rejects.toThrow();
  });
});

const invalidCargo = [
  { name: "dangling transitive dependency", value: cargoLock(cargoBlocks.filter((block) => !block.includes('name = "native-leaf"'))) },
  { name: "ambiguous dependency without version/source", value: cargoLock(cargoBlocks.map((block, index) => index === 0 ? block.replace(`twin 1.0.0 (${registry})`, "twin") : block)) },
  { name: "malformed TOML", value: 'version = 4\n[[package]\nname = "broken"\n' },
  { name: "invalid registry checksum", value: cargoLock().replace("a".repeat(64), "not-a-checksum") },
  { name: "missing registry checksum", value: cargoLock().replace(`checksum = "${"a".repeat(64)}"\n`, "") },
];
for (const scenario of invalidCargo) {
  test(`incomplete Cargo graph fails closed: ${scenario.name}`, async () => {
    await withFixture(async ({ source, materials }) => {
      await expect(generateSbom(source, materials)).rejects.toThrow();
    }, { cargo: scenario.value });
  });
}
for (const scenario of ["dangling", "missing-direct", "missing-optional", "malformed", "bad-integrity", "unsupported-version"] as const) {
  test(`incomplete Bun graph fails closed: ${scenario}`, async () => {
    const value = bunValue();
    if (scenario === "dangling") delete (value.packages as Record<string, unknown>)["bridge/js-leaf"];
    if (scenario === "missing-direct") delete (value.packages as Record<string, unknown>).renderer;
    if (scenario === "missing-optional") delete (value.packages as Record<string, unknown>).image;
    if (scenario === "bad-integrity") value.packages.renderer[3] = "sha512-not-base64!";
    if (scenario === "unsupported-version") value.lockfileVersion = 999;
    await withFixture(async ({ source, materials }) => {
      await expect(generateSbom(source, materials)).rejects.toThrow();
    }, { bun: scenario === "malformed" ? '{"lockfileVersion":2,"packages": ' : JSON.stringify(value) });
  });
}

test("equivalent selected inputs yield byte-identical BOMs across calls, input order and checkout location", async () => {
  const first = await fixture();
  let second: Fixture | undefined;
  try {
    second = await fixture();
    const original = JSON.stringify(await generateSbom(first.source, first.materials));
    const again = JSON.stringify(await generateSbom(first.source, first.materials));
    const reordered = JSON.stringify(await generateSbom(first.source, [...first.materials].reverse()));
    const relocated = JSON.stringify(await generateSbom(second.source, second.materials));
    expect(again).toBe(original);
    expect(reordered).toBe(original);
    expect(relocated).toBe(original);
    expect(original).not.toContain(first.root);
    expect(original).not.toContain(second.root);
  } finally {
    await rm(first.root, { recursive: true, force: true });
    if (second) await rm(second.root, { recursive: true, force: true });
  }
});

test("changing a pinned transitive archive checksum changes that component's emitted integrity", async () => {
  await withFixture(async ({ root, source, materials }) => {
    const before: Bom = await generateSbom(source, materials);
    await Bun.write(join(root, "Cargo.lock"), cargoLock().replace("b".repeat(64), "f".repeat(64)));
    const recaptured = await captureSourceIdentity(root);
    const after: Bom = await generateSbom(recaptured, materials);
    expect(one(before, "native-leaf", "4.0.0").hashes).toContainEqual({ alg: "SHA-256", content: "b".repeat(64) });
    expect(one(after, "native-leaf", "4.0.0").hashes).toContainEqual({ alg: "SHA-256", content: "f".repeat(64) });
    expect(one(after, "native-leaf", "4.0.0").hashes).not.toContainEqual({ alg: "SHA-256", content: "b".repeat(64) });
  });
});
