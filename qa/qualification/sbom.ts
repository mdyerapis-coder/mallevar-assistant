import { createHash } from "node:crypto";
import { readFile, realpath } from "node:fs/promises";
import { createRequire } from "node:module";
import { isAbsolute, relative, resolve } from "node:path";
import type { SourceIdentity } from "./source-identity";

const require = createRequire(import.meta.url);
const ts = require("typescript") as {
  parseConfigFileTextToJson(fileName: string, jsonText: string): {
    config?: unknown;
    error?: { messageText: string | { messageText: string; next?: { messageText: string }[] } };
  };
  flattenDiagnosticMessageText(messageText: string | { messageText: string; next?: { messageText: string }[] }, newLine: string): string;
};

export interface MaterialIdentity {
  name: string;
  version: string;
  ecosystem: string;
  source: string;
  sha256: string;
  purl?: string;
  license?: string;
}

export interface Component {
  "bom-ref"?: string;
  type?: string;
  name: string;
  version?: string;
  group?: string;
  purl?: string;
  description?: string;
  author?: string;
  supplier?: { name?: string };
  hashes?: { alg: string; content: string }[];
  licenses?: { license?: { id?: string; name?: string }; expression?: string }[];
  externalReferences?: { type: string; url: string }[];
  properties?: { name: string; value: string }[];
  components?: Component[];
}

export interface CycloneDxBom {
  bomFormat: string;
  specVersion: string;
  version: number;
  metadata?: { component?: Component };
  components?: Component[];
  dependencies?: { ref: string; dependsOn?: string[] }[];
  annotations?: { subjects: string[]; text: string }[];
}

const HEX64 = /^[0-9a-f]{64}$/;
const SPDX_ID = /^[A-Za-z0-9][A-Za-z0-9.-]*$/;
const CARGO_NAME = /^[A-Za-z0-9_-]+$/;
const CARGO_DEP = /^([A-Za-z0-9_-]+)(?: ([^ ()]+))?(?: \((.+)\))?$/;

interface CargoPackage {
  name: string;
  version: string;
  source?: string;
  checksum?: string;
  dependencies: string[];
}

interface SemVer {
  major: number;
  minor: number;
  patch: number;
  prerelease: string[];
}

interface BunPackage {
  key: string;
  name: string;
  version: string;
  workspace?: string;
  integrity?: string;
  dependencies: Record<string, string>;
  optionalDependencies: Record<string, string>;
  devDependencies: Record<string, string>;
  os?: string;
  cpu?: string;
}

class Fail extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SbomError";
  }
}

function fail(message: string): never {
  throw new Fail(message);
}

function sorted<T>(values: readonly T[], key: (value: T) => string): T[] {
  return [...values].sort((left, right) => key(left) < key(right) ? -1 : key(left) > key(right) ? 1 : 0);
}

function encodePurl(value: string): string {
  return encodeURIComponent(value).replace(/%40/g, "%40");
}

function npmName(name: string): string {
  return name.startsWith("@") ? `%40${name.slice(1)}` : encodeURIComponent(name);
}

function unknownLicense(): Pick<Component, "description" | "licenses"> {
  return {
    description: "License not asserted",
    licenses: [{ expression: "NOASSERTION" }],
  };
}

function knownLicense(license: string): Pick<Component, "licenses"> {
  if (/^(NOASSERTION|NONE)$/i.test(license)) return { licenses: [{ expression: license.toUpperCase() }] };
  if (SPDX_ID.test(license)) return { licenses: [{ license: { id: license } }] };
  return { licenses: [{ expression: license }] };
}

function componentBase(ref: string, type: string, name: string, version: string | undefined, license?: string): Component {
  const component: Component = { "bom-ref": ref, type, name };
  if (version !== undefined) component.version = version;
  if (license === undefined) Object.assign(component, unknownLicense());
  else Object.assign(component, knownLicense(license));
  return component;
}

async function boundBytes(source: SourceIdentity, path: string): Promise<Buffer> {
  if (!path || isAbsolute(path) || path.includes("\\") || path.includes("\0") || path.split("/").some((part) => part === "" || part === "." || part === "..")) {
    fail(`selected input ${JSON.stringify(path)} is not a normalized repository-relative path`);
  }
  const entry = source.files.find((file) => file.path === path);
  if (!entry) fail(`selected source inventory does not bind ${path}`);
  const root = await realpath(source.root);
  let actual: string;
  try {
    actual = await realpath(resolve(root, path));
  } catch (error) {
    fail(`selected input ${path} is missing: ${error instanceof Error ? error.message : String(error)}`);
  }
  const rel = relative(root, actual);
  if (rel === "" || rel === ".." || rel.startsWith("../") || isAbsolute(rel)) fail(`selected input ${path} escapes the source root`);
  const bytes = await readFile(actual);
  const observed = createHash("sha256").update(bytes).digest("hex");
  if (observed !== entry.sha256) fail(`selected input ${path} changed after source capture`);
  return bytes;
}

class CargoParser {
  private index = 0;
  constructor(private readonly text: string) {}

  parse(): { version: number; packages: CargoPackage[] } {
    let version: number | undefined;
    const packages: CargoPackage[] = [];
    this.skip();
    while (!this.done()) {
      if (this.consume("[[package]]")) {
        packages.push(this.package());
        continue;
      }
      if (this.peek() === "[") fail("Cargo.lock contains an unsupported table");
      const key = this.key();
      this.expect("=");
      this.skip();
      if (key === "version" && version === undefined) version = this.integer();
      else fail(`unexpected Cargo.lock field ${key}`);
      this.skip();
    }
    if (version !== 4) fail(`unsupported Cargo.lock version ${version ?? "missing"}`);
    if (packages.length === 0) fail("Cargo.lock contains no packages");
    return { version, packages };
  }

  private package(): CargoPackage {
    const fields = new Map<string, string | string[]>();
    this.skip();
    while (!this.done() && !this.starts("[[") && this.peek() !== "[") {
      const key = this.key();
      this.expect("=");
      this.skip();
      fields.set(key, this.peek() === "[" ? this.stringArray() : this.string());
      this.skip();
    }
    const name = fields.get("name");
    const version = fields.get("version");
    const source = fields.get("source");
    const checksum = fields.get("checksum");
    const dependencies = fields.get("dependencies") ?? [];
    if (typeof name !== "string" || !CARGO_NAME.test(name)) fail("Cargo package is missing a valid name");
    if (typeof version !== "string" || version === "") fail(`Cargo package ${name} is missing a version`);
    if (source !== undefined && typeof source !== "string") fail(`Cargo package ${name} has a malformed source`);
    if (checksum !== undefined && typeof checksum !== "string") fail(`Cargo package ${name} has a malformed checksum`);
    if (!Array.isArray(dependencies) || dependencies.some((item) => typeof item !== "string")) fail(`Cargo package ${name} has malformed dependencies`);
    if (source !== undefined && (checksum === undefined || !HEX64.test(checksum))) fail(`Cargo package ${name}@${version} is missing a valid registry checksum`);
    if (checksum !== undefined && !HEX64.test(checksum)) fail(`Cargo package ${name}@${version} has an invalid checksum`);
    for (const spec of dependencies) if (!CARGO_DEP.test(spec)) fail(`malformed Cargo dependency ${spec}`);
    return { name, version, source, checksum, dependencies };
  }

  private stringArray(): string[] {
    const values: string[] = [];
    this.expect("[");
    this.skip();
    while (!this.consume("]")) {
      values.push(this.string());
      this.skip();
      this.consume(",");
      this.skip();
      if (this.done()) fail("unclosed Cargo.lock array");
    }
    return values;
  }

  private string(): string {
    this.expect("\"");
    let value = "";
    while (!this.done()) {
      const char = this.text[this.index++];
      if (char === "\"") return value;
      if (char === "\\") {
        const escaped = this.text[this.index++];
        if (escaped === "n") value += "\n";
        else if (escaped === "t") value += "\t";
        else if (escaped === "r") value += "\r";
        else if (escaped === "\\" || escaped === "\"") value += escaped;
        else fail("unsupported Cargo.lock string escape");
      } else if (char === "\n") fail("unterminated Cargo.lock string");
      else value += char;
    }
    fail("unterminated Cargo.lock string");
  }

  private integer(): number {
    const start = this.index;
    while (/[0-9]/.test(this.peek())) this.index += 1;
    if (start === this.index) fail("expected Cargo.lock integer");
    return Number(this.text.slice(start, this.index));
  }

  private key(): string {
    const start = this.index;
    while (/[A-Za-z0-9_-]/.test(this.peek())) this.index += 1;
    if (start === this.index) fail(`unexpected Cargo.lock token at ${this.index}`);
    const key = this.text.slice(start, this.index);
    this.skipSpaces();
    return key;
  }

  private expect(token: string): void {
    if (!this.consume(token)) fail(`expected ${token} in Cargo.lock`);
  }

  private consume(token: string): boolean {
    if (!this.starts(token)) return false;
    this.index += token.length;
    return true;
  }

  private starts(token: string): boolean {
    return this.text.startsWith(token, this.index);
  }

  private peek(): string {
    return this.text[this.index] ?? "";
  }

  private done(): boolean {
    return this.index >= this.text.length;
  }

  private skipSpaces(): void {
    while (this.peek() === " " || this.peek() === "\t") this.index += 1;
  }

  private skip(): void {
    while (!this.done()) {
      const char = this.peek();
      if (char === " " || char === "\t" || char === "\r" || char === "\n") this.index += 1;
      else if (char === "#") {
        while (!this.done() && this.peek() !== "\n") this.index += 1;
      } else return;
    }
  }
}

function parseCargoDep(spec: string): { name: string; version?: string; source?: string } {
  const match = CARGO_DEP.exec(spec);
  if (!match) fail(`malformed Cargo dependency ${spec}`);
  return { name: match[1] ?? "", version: match[2], source: match[3] };
}

function resolveCargo(spec: string, packages: readonly CargoPackage[]): CargoPackage {
  const wanted = parseCargoDep(spec);
  let matches = packages.filter((pkg) => pkg.name === wanted.name);
  if (wanted.version !== undefined) matches = matches.filter((pkg) => pkg.version === wanted.version);
  if (wanted.source !== undefined) matches = matches.filter((pkg) => pkg.source === wanted.source);
  if (matches.length === 0) fail(`dangling Cargo dependency ${spec}`);
  if (matches.length !== 1) fail(`ambiguous Cargo dependency ${spec}`);
  const found = matches[0];
  if (!found) fail(`dangling Cargo dependency ${spec}`);
  return found;
}

function cargoIdentity(pkg: CargoPackage): string {
  return `${pkg.name}\0${pkg.version}\0${pkg.source ?? ""}`;
}

function cargoPurl(pkg: CargoPackage): string {
  const base = `pkg:cargo/${pkg.name}@${pkg.version}`;
  if (pkg.source === undefined) return base;
  if (!pkg.source.startsWith("registry+")) fail(`unsupported Cargo source ${pkg.source}`);
  const url = pkg.source.slice("registry+".length);
  if (!url.startsWith("https://")) fail(`unsupported Cargo registry ${pkg.source}`);
  return `${base}?repository_url=${encodeURIComponent(url)}`;
}

function cargoReferences(pkg: CargoPackage): Component["externalReferences"] {
  if (pkg.source === undefined) return undefined;
  const url = pkg.source.startsWith("registry+") ? pkg.source.slice("registry+".length) : pkg.source;
  return [{ type: "distribution", url }];
}

function parseVersion(raw: string): SemVer | undefined {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(raw);
  if (!match) return undefined;
  return { major: Number(match[1]), minor: Number(match[2]), patch: Number(match[3]), prerelease: match[4] ? match[4].split(".") : [] };
}

function comparePrerelease(left: readonly string[], right: readonly string[]): number {
  const count = Math.max(left.length, right.length);
  for (let index = 0; index < count; index += 1) {
    const a = left[index];
    const b = right[index];
    if (a === undefined) return -1;
    if (b === undefined) return 1;
    const aNum = /^[0-9]+$/.test(a);
    const bNum = /^[0-9]+$/.test(b);
    if (aNum && bNum) {
      const diff = Number(a) - Number(b);
      if (diff !== 0) return diff;
    } else if (aNum !== bNum) return aNum ? -1 : 1;
    else if (a !== b) return a < b ? -1 : 1;
  }
  return 0;
}

function compareVersion(left: SemVer, right: SemVer): number {
  if (left.major !== right.major) return left.major - right.major;
  if (left.minor !== right.minor) return left.minor - right.minor;
  if (left.patch !== right.patch) return left.patch - right.patch;
  if (left.prerelease.length === 0 && right.prerelease.length === 0) return 0;
  if (left.prerelease.length === 0) return 1;
  if (right.prerelease.length === 0) return -1;
  return comparePrerelease(left.prerelease, right.prerelease);
}

function satisfies(version: string, range: string, workspace: boolean): boolean {
  const alternatives = range.split("||").map((part) => part.trim()).filter(Boolean);
  if (alternatives.length === 0) fail(`unsupported bun version range ${range}`);
  return alternatives.some((alternative) => satisfiesAnd(version, alternative, workspace));
}

function satisfiesAnd(version: string, range: string, workspace: boolean): boolean {
  if (range === "*" || range === "x" || range === "X") return true;
  if (range === "workspace:*") return workspace;
  if (range.startsWith("workspace:")) return workspace;
  const parsed = parseVersion(version);
  if (!parsed) fail(`unsupported bun package version ${version}`);
  const comparators = range.split(/\s+/).filter(Boolean);
  return comparators.every((comparator) => satisfiesComparator(parsed, comparator));
}

function satisfiesComparator(version: SemVer, comparator: string): boolean {
  const operator = /^(>=|<=|>|<|=|\^|~)?/.exec(comparator)?.[1] ?? "";
  const raw = comparator.slice(operator.length);
  if (raw === "*" || raw === "x" || raw === "X") return operator === "" || operator === "=";
  const partial = /^(\d+)(?:\.(\d+|x|X|\*))?(?:\.(\d+|x|X|\*))?(?:-([0-9A-Za-z.-]+))?$/.exec(raw);
  if (!partial) fail(`unsupported bun version range ${comparator}`);
  const major = Number(partial[1]);
  const minorRaw = partial[2];
  const patchRaw = partial[3];
  const prerelease = partial[4] ? partial[4].split(".") : [];
  const minor = minorRaw === undefined || minorRaw === "x" || minorRaw === "X" || minorRaw === "*" ? undefined : Number(minorRaw);
  const patch = patchRaw === undefined || patchRaw === "x" || patchRaw === "X" || patchRaw === "*" ? undefined : Number(patchRaw);
  const floor: SemVer = { major, minor: minor ?? 0, patch: patch ?? 0, prerelease };
  if (operator === "" || operator === "=") {
    if (minor === undefined || patch === undefined) return version.major === major && (minor === undefined || version.minor === minor) && version.prerelease.length === 0;
    return compareVersion(version, floor) === 0;
  }
  if (operator === ">") return compareVersion(version, floor) > 0;
  if (operator === ">=") return compareVersion(version, floor) >= 0;
  if (operator === "<") return compareVersion(version, floor) < 0 && (floor.prerelease.length > 0 || version.prerelease.length === 0);
  if (operator === "<=") return compareVersion(version, floor) <= 0;
  if (operator === "^") {
    const ceiling: SemVer = major > 0 || minor === undefined
      ? { major: major + 1, minor: 0, patch: 0, prerelease: [] }
      : minor > 0 || patch === undefined
        ? { major, minor: minor + 1, patch: 0, prerelease: [] }
        : { major, minor, patch: (patch ?? 0) + 1, prerelease: [] };
    return compareVersion(version, floor) >= 0 && compareVersion(version, ceiling) < 0 && (prerelease.length > 0 || version.prerelease.length === 0);
  }
  if (operator === "~") {
    const ceiling: SemVer = minor === undefined
      ? { major: major + 1, minor: 0, patch: 0, prerelease: [] }
      : { major, minor: (minor ?? 0) + 1, patch: 0, prerelease: [] };
    return compareVersion(version, floor) >= 0 && compareVersion(version, ceiling) < 0 && (prerelease.length > 0 || version.prerelease.length === 0);
  }
  fail(`unsupported bun version range ${comparator}`);
}

function parseJsonc(text: string): unknown {
  const parsed = ts.parseConfigFileTextToJson("bun.lock", text);
  if (parsed.error || parsed.config === undefined) {
    const message = parsed.error ? ts.flattenDiagnosticMessageText(parsed.error.messageText, "\n") : "empty document";
    fail(`bun.lock is not valid JSONC: ${message}`);
  }
  return parsed.config;
}

function stringMap(value: unknown, label: string): Record<string, string> {
  if (value === undefined) return {};
  if (typeof value !== "object" || value === null || Array.isArray(value)) fail(`${label} is malformed`);
  const result: Record<string, string> = {};
  for (const [key, item] of Object.entries(value)) {
    if (typeof item !== "string" || item === "") fail(`${label}.${key} is malformed`);
    result[key] = item;
  }
  return result;
}

function platform(value: unknown, label: string): string | undefined {
  if (value === undefined) return undefined;
  const items = Array.isArray(value) ? value : [value];
  if (items.some((item) => typeof item !== "string" || item === "")) fail(`${label} is malformed`);
  return (items as string[]).join(",");
}

function splitIdent(ident: string): { name: string; spec: string } {
  const at = ident.lastIndexOf("@");
  if (at <= 0) fail(`malformed bun package ident ${ident}`);
  return { name: ident.slice(0, at), spec: ident.slice(at + 1) };
}

function packageSegments(key: string): string[] {
  const parts = key.split("/");
  const segments: string[] = [];
  for (let index = 0; index < parts.length; index += 1) {
    const part = parts[index];
    if (!part) fail(`malformed bun package key ${key}`);
    if (part.startsWith("@")) {
      const name = parts[index + 1];
      if (!name) fail(`malformed scoped bun package key ${key}`);
      segments.push(`${part}/${name}`);
      index += 1;
    } else segments.push(part);
  }
  return segments;
}

function parentKey(key: string): string {
  const segments = packageSegments(key);
  segments.pop();
  return segments.join("/");
}

function sriHex(integrity: string, label: string): string {
  const match = /^sha512-([A-Za-z0-9+/]+={0,2})$/.exec(integrity);
  if (!match?.[1]) fail(`invalid bun integrity for ${label}`);
  const bytes = Buffer.from(match[1], "base64");
  if (bytes.length !== 64 || bytes.toString("base64") !== match[1]) fail(`invalid bun integrity for ${label}`);
  return bytes.toString("hex");
}

function parseBun(text: string): { workspaces: BunPackage[]; packages: BunPackage[] } {
  const value = parseJsonc(text);
  if (typeof value !== "object" || value === null || Array.isArray(value)) fail("bun.lock must be an object");
  const record = value as Record<string, unknown>;
  if (record.lockfileVersion !== 1 && record.lockfileVersion !== 2) fail(`unsupported bun.lock version ${String(record.lockfileVersion)}`);
  if (typeof record.workspaces !== "object" || record.workspaces === null || Array.isArray(record.workspaces)) fail("bun.lock workspaces are malformed");
  if (typeof record.packages !== "object" || record.packages === null || Array.isArray(record.packages)) fail("bun.lock packages are malformed");
  const workspaces: BunPackage[] = [];
  for (const [path, raw] of Object.entries(record.workspaces)) {
    if (typeof raw !== "object" || raw === null || Array.isArray(raw)) fail(`bun workspace ${path} is malformed`);
    const workspace = raw as Record<string, unknown>;
    if (typeof workspace.name !== "string" || workspace.name === "") fail(`bun workspace ${path} is missing a name`);
    if (typeof workspace.version !== "string" || workspace.version === "") fail(`bun workspace ${path || "<root>"} is missing a version`);
    workspaces.push({
      key: path === "" ? "" : workspace.name,
      name: workspace.name,
      version: workspace.version,
      workspace: path,
      dependencies: stringMap(workspace.dependencies, `workspace ${path} dependencies`),
      optionalDependencies: stringMap(workspace.optionalDependencies, `workspace ${path} optionalDependencies`),
      devDependencies: stringMap(workspace.devDependencies, `workspace ${path} devDependencies`),
    });
  }
  if (!workspaces.some((workspace) => workspace.workspace === "")) fail("bun.lock is missing the root workspace");
  const packages: BunPackage[] = [];
  const links = new Map<string, string>();
  for (const [key, raw] of Object.entries(record.packages)) {
    if (!Array.isArray(raw) || raw.length < 1 || typeof raw[0] !== "string") fail(`bun package ${key} is malformed`);
    const ident = splitIdent(raw[0]);
    if (ident.spec.startsWith("workspace:")) {
      if (raw.length !== 1) fail(`bun workspace link ${key} is malformed`);
      links.set(key, ident.spec.slice("workspace:".length));
      continue;
    }
    if (raw.length < 4 || typeof raw[1] !== "string" || typeof raw[2] !== "object" || raw[2] === null || Array.isArray(raw[2]) || typeof raw[3] !== "string") {
      fail(`bun package ${key} is incomplete`);
    }
    const info = raw[2] as Record<string, unknown>;
    const version = parseVersion(ident.spec);
    if (!version) fail(`bun package ${key} has an unsupported version`);
    packages.push({
      key,
      name: ident.name,
      version: ident.spec,
      integrity: raw[3],
      dependencies: stringMap(info.dependencies, `${key} dependencies`),
      optionalDependencies: stringMap(info.optionalDependencies, `${key} optionalDependencies`),
      devDependencies: stringMap(info.devDependencies, `${key} devDependencies`),
      os: platform(info.os, `${key} os`),
      cpu: platform(info.cpu, `${key} cpu`),
    });
  }
  for (const [key, path] of links) {
    if (!workspaces.some((workspace) => workspace.workspace === path && workspace.name === key)) fail(`dangling bun workspace link ${key}`);
  }
  for (const workspace of workspaces) {
    if (workspace.workspace !== "" && !links.has(workspace.name)) fail(`bun workspace ${workspace.name} is missing its package link`);
  }
  return { workspaces, packages };
}

function resolveBun(fromKey: string, name: string, range: string, packages: readonly BunPackage[], workspaces: readonly BunPackage[]): BunPackage {
  let cursor = fromKey;
  const seen = new Set<string>();
  while (true) {
    const key = cursor === "" ? name : `${cursor}/${name}`;
    const found = packages.find((pkg) => pkg.key === key) ?? (cursor === "" ? workspaces.find((pkg) => pkg.name === name) : undefined);
    if (found) {
      if (!satisfies(found.version, range, found.workspace !== undefined)) fail(`resolved ${key}@${found.version} does not satisfy ${name}@${range}`);
      return found;
    }
    if (cursor === "" || seen.has(cursor)) break;
    seen.add(cursor);
    cursor = parentKey(cursor);
  }
  fail(`dangling bun dependency ${name}@${range} from ${fromKey || "<root>"}`);
}

function bunEdges(pkg: BunPackage, packages: readonly BunPackage[], workspaces: readonly BunPackage[]): string[] {
  const declared = [
    ...Object.entries(pkg.dependencies),
    ...Object.entries(pkg.optionalDependencies),
    ...Object.entries(pkg.devDependencies),
  ];
  const refs = declared.map(([name, range]) => bunRef(resolveBun(pkg.key, name, range, packages, workspaces)));
  if (new Set(refs).size !== refs.length) fail(`duplicate bun dependency from ${pkg.name}@${pkg.version}`);
  return sorted(refs, (ref) => ref);
}

function bunRef(pkg: BunPackage): string {
  return `pkg:npm/${npmName(pkg.name)}@${pkg.version}`;
}

function cargoRef(pkg: CargoPackage): string {
  return cargoPurl(pkg);
}

function materialRef(material: MaterialIdentity): string {
  if (material.purl) return material.purl;
  return `pkg:${encodeURIComponent(material.ecosystem)}/${encodePurl(material.name)}@${encodePurl(material.version)}?source=${encodeURIComponent(material.source)}`;
}

export async function generateSbom(source: SourceIdentity, materials: readonly MaterialIdentity[]): Promise<CycloneDxBom> {
  if (typeof source?.root !== "string" || !Array.isArray(source.files)) fail("source identity is incomplete");
  const cargoText = (await boundBytes(source, "Cargo.lock")).toString("utf8");
  const bunText = (await boundBytes(source, "bun.lock")).toString("utf8");
  const cargo = new CargoParser(cargoText).parse();
  const identities = new Set<string>();
  for (const pkg of cargo.packages) {
    const identity = cargoIdentity(pkg);
    if (identities.has(identity)) fail(`duplicate Cargo identity ${pkg.name}@${pkg.version}`);
    identities.add(identity);
  }
  const bun = parseBun(bunText);
  const orderedMaterials = sorted(materials, (material) => `${material.name}\0${material.version}\0${material.source}`);
  const materialComponents: Component[] = [];
  for (const material of orderedMaterials) {
    if (!HEX64.test(material.sha256)) fail(`material ${material.name}@${material.version} digest is malformed`);
    const observed = createHash("sha256").update(await boundBytes(source, material.source)).digest("hex");
    if (observed !== material.sha256) fail(`material ${material.name}@${material.version} digest does not match observed bytes`);
    const ref = materialRef(material);
    const component = componentBase(ref, "file", material.name, material.version, material.license);
    component.purl = material.purl ?? ref;
    component.hashes = [{ alg: "SHA-256", content: observed }];
    materialComponents.push(component);
  }
  const cargoComponents = sorted(cargo.packages, (pkg) => cargoRef(pkg)).map((pkg): Component => {
    const component = componentBase(cargoRef(pkg), "library", pkg.name, pkg.version);
    component.purl = cargoRef(pkg);
    if (pkg.checksum) component.hashes = [{ alg: "SHA-256", content: pkg.checksum }];
    const references = cargoReferences(pkg);
    if (references) component.externalReferences = references;
    return component;
  });
  const bunNodes = [...bun.workspaces, ...bun.packages];
  const bunComponents = sorted(bunNodes, (pkg) => bunRef(pkg)).map((pkg): Component => {
    const component = componentBase(bunRef(pkg), "library", pkg.name, pkg.version);
    component.purl = bunRef(pkg);
    if (pkg.integrity) component.hashes = [{ alg: "SHA-512", content: sriHex(pkg.integrity, pkg.name) }];
    const properties: { name: string; value: string }[] = [];
    if (pkg.os) properties.push({ name: "cdx:npm:os", value: pkg.os });
    if (pkg.cpu) properties.push({ name: "cdx:npm:cpu", value: pkg.cpu });
    if (pkg.workspace !== undefined) properties.push({ name: "cdx:npm:workspace", value: pkg.workspace || "." });
    if (properties.length > 0) component.properties = properties;
    return component;
  });
  const components = [...cargoComponents, ...bunComponents, ...materialComponents];
  const refs = components.map((component) => component["bom-ref"] ?? "");
  if (refs.some((ref) => ref === "") || new Set(refs).size !== refs.length) fail("component identities are not unique");
  const dependencies = [
    ...sorted(cargo.packages, (pkg) => cargoRef(pkg)).map((pkg) => ({
      ref: cargoRef(pkg),
      dependsOn: sorted(pkg.dependencies.map((spec) => cargoRef(resolveCargo(spec, cargo.packages))), (ref) => ref),
    })),
    ...sorted(bunNodes, (pkg) => bunRef(pkg)).map((pkg) => ({
      ref: bunRef(pkg),
      dependsOn: bunEdges(pkg, bun.packages, bun.workspaces),
    })),
    ...materialComponents.map((component) => ({ ref: component["bom-ref"] ?? "", dependsOn: [] as string[] })),
  ];
  const releaseRef = "pkg:generic/ironbark-release@1";
  if (refs.includes(releaseRef)) fail("release component identity collides with a selected package");
  const releaseDepends = sorted([
    ...cargo.packages.filter((pkg) => pkg.source === undefined).map(cargoRef),
    ...bun.workspaces.map(bunRef),
    ...materialComponents.map((component) => component["bom-ref"] ?? ""),
  ], (ref) => ref);
  dependencies.push({ ref: releaseRef, dependsOn: releaseDepends });
  dependencies.sort((left, right) => left.ref < right.ref ? -1 : left.ref > right.ref ? 1 : 0);
  const release = componentBase(releaseRef, "application", "ironbark-release", "1");
  release.description = "Release aggregate of pinned Cargo, Bun and shipped material identities; license not asserted";
  return {
    bomFormat: "CycloneDX",
    specVersion: "1.6",
    version: 1,
    metadata: { component: release },
    components: sorted(components, (component) => component["bom-ref"] ?? ""),
    dependencies,
  };
}
