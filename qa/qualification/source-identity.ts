import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat, readFile, readlink, realpath } from "node:fs/promises";
import { execFile } from "node:child_process";
import { isAbsolute, relative, resolve } from "node:path";
import { promisify } from "node:util";

export interface SourceFileIdentity { path: string; mode: string; sha256: string }
export interface SourceIdentity {
  root: string;
  commit: string;
  tree: string;
  source_digest: string;
  inventory_digest: string;
  dirty: boolean;
  files: SourceFileIdentity[];
  toolchain: Record<string, string>;
  environment: Record<string, string>;
}

// These baseline requirements cannot be removed by a manifest override. Additional
// ignored inputs must be individually selected with source_inventory.required.
const REQUIRED = ["Cargo.toml", "Cargo.lock", "package.json", "bun.lock", "packaging/manifest.json",
  "packages/contracts/src/generated.ts", "packages/contracts/schema.json"];
const UNTRACKED_ROOTS = ["src/", "crates/", "packages/", "apps/", "qa/", "packaging/", "tests/", "integrations/", "docs/", ".github/", ".cargo/"];
const AUTOMATIC_INPUTS = ["bunfig.toml", ".bunfig.toml", ".cargo/config", ".cargo/config.toml", "rust-toolchain", "rust-toolchain.toml", "tsconfig.json"];
const PRIVATE_ENV_INPUTS = [".env", ".env.local", ".env.development", ".env.test", ".env.production", ".env.development.local", ".env.test.local", ".env.production.local"];
// Explicit non-source inventory. Do not exclude product files based on dirty state.
const EXCLUDED_DIRECTORIES: Record<string, true> = {
  ".git": true, "node_modules": true, "target": true, ".superpowers": true,
  ".sources": true, ".cache": true, "dist": true, "coverage": true, ".worktrees": true,
};
const EXCLUDED_ROOTS = ["evidence/", "profiles/", ".profiles/", ".private/", "private/", "qa/private/", ".cargo/registry/", ".cargo/git/", ".cargo/credentials"];
const exec = promisify(execFile);
const HEX = /^[0-9a-f]{64}$/;
export const identityDigest = (value: unknown): string => createHash("sha256").update(JSON.stringify(value)).digest("hex");

/** The exact environment delivered to gates; undeclared parent selectors never leak through. */
export function gateEnvironment(): Record<string, string> {
  const selected: Record<string, string> = { RUSTUP_AUTO_INSTALL: "0" };
  for (const key of [
    "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "CI", "SOURCE_DATE_EPOCH",
    "RUSTUP_TOOLCHAIN", "RUSTUP_HOME", "CARGO_HOME", "CARGO_BUILD_TARGET", "CARGO_INCREMENTAL", "CARGO_TARGET_DIR",
    "RUSTC", "RUSTDOC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTDOCFLAGS",
    "BUN_INSTALL", "BUN_OPTIONS", "NODE_OPTIONS", "CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "LDFLAGS",
    "IRONBARK_CORE_BIN", "IRONBARK_CLI_BIN", "IRONBARK_TEST_BUN", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR",
    "DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "XAUTHORITY", "XDG_SESSION_TYPE",
  ]) {
    const value = process.env[key];
    if (value !== undefined) selected[key] = value;
  }
  return selected;
}

function normalized(path: string): string {
  if (!path || isAbsolute(path) || path.includes("\\") || path.includes("\0") || path.split("/").some((part) => part === "." || part === ".." || part === "")) {
    throw new Error(`unsafe source inventory path ${JSON.stringify(path)}; select normalized repository-relative files`);
  }
  return path;
}
function excluded(path: string): boolean {
  return path.split("/").some((part) => EXCLUDED_DIRECTORIES[part] === true || part === ".env") || EXCLUDED_ROOTS.some((prefix) => path.startsWith(prefix));
}
function inside(root: string, path: string): boolean {
  const rel = relative(root, path);
  return rel !== "" && rel !== ".." && !rel.startsWith("../") && !isAbsolute(rel);
}
async function git(root: string, args: string[], environment: Record<string, string>): Promise<string> {
  const result = await exec("git", ["-C", root, ...args], { env: environment, encoding: "utf8", maxBuffer: 32 * 1024 * 1024 });
  return result.stdout;
}
async function fileHash(path: string): Promise<string> {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
}
async function toolchain(root: string, environment: Record<string, string>): Promise<Record<string, string>> {
  const result: Record<string, string> = {};
  const digests = new Map<string, string>();
  for (const [name, args] of [["git", ["--version"]], ["bun", ["--version"]], ["cargo", ["--version"]], ["rustc", ["-vV"]]] as const) {
    const executable = Bun.which(name, { PATH: environment.PATH ?? "/usr/bin:/bin", cwd: root });
    if (!executable) throw new Error(`required toolchain executable ${name} is missing; install the repository-pinned toolchain before running gates`);
    const path = await realpath(executable);
    // Invoke the proxy by its selected name: resolving rustc/cargo symlinks
    // before execution would invoke rustup instead of the requested tool.
    const observed = await exec(executable, [...args], { cwd: root, env: environment, encoding: "utf8", maxBuffer: 65536 });
    result[name] = observed.stdout.trim();
    result[`${name}_path`] = path;
    result[`${name}_invocation_path`] = executable;
    let digest = digests.get(path);
    if (digest === undefined) {
      digest = await fileHash(path);
      digests.set(path, digest);
    }
    result[`${name}_sha256`] = digest;
  }
  result.runner_bun = Bun.version;
  result.runner_path = await realpath(process.execPath);
  result.runner_sha256 = digests.get(result.runner_path) ?? await fileHash(result.runner_path);
  return result;
}

/** Git plumbing selects tracked files; explicit product roots select new inputs.
 * Required manifest inputs are read even when ignored. No status/diff loop, no
 * reading private/evidence/cache trees and no material copied into receipts. */
export async function captureSourceIdentity(root: string, selectedEnvironment = gateEnvironment()): Promise<SourceIdentity> {
  root = await realpath(root);
  for (const path of PRIVATE_ENV_INPUTS) {
    try { await lstat(resolve(root, path)); }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") continue;
      throw error;
    }
    throw new Error(`automatic private environment input ${path} blocks managed gates; use a clean qualification checkout with explicitly selected environment, without copying private dotenv material`);
  }
  const gitRoot = (await git(root, ["rev-parse", "--show-toplevel"], selectedEnvironment)).trim();
  if (await realpath(gitRoot) !== root) throw new Error("source root must be the Git checkout root; run gates from the repository root");
  const commit = (await git(root, ["rev-parse", "HEAD"], selectedEnvironment)).trim();
  const tree = (await git(root, ["rev-parse", "HEAD^{tree}"], selectedEnvironment)).trim();
  const manifest: unknown = JSON.parse(await readFile(resolve(root, "packaging/manifest.json"), "utf8"));
  if (typeof manifest !== "object" || manifest === null || Array.isArray(manifest)) throw new Error("source manifest must be an object; restore packaging/manifest.json");
  const policy = (manifest as Record<string, unknown>).source_inventory;
  const required = new Set(REQUIRED);
  const roots = new Set(UNTRACKED_ROOTS);
  if (policy !== undefined) {
    if (typeof policy !== "object" || policy === null || Array.isArray(policy)) throw new Error("source_inventory must be an object; declare required files and untracked_roots");
    for (const [key, destination] of [["required", required], ["untracked_roots", roots]] as const) {
      const values = (policy as Record<string, unknown>)[key];
      if (!Array.isArray(values) || values.some((item) => typeof item !== "string")) throw new Error(`source_inventory.${key} must be a string array; declare exact input paths`);
      for (const value of values as string[]) {
        const path = key === "untracked_roots" ? `${normalized(value.replace(/\/$/, ""))}/` : normalized(value);
        if (excluded(path)) throw new Error(`required source input ${path} selects an excluded private/build/evidence path; correct the inventory`);
        destination.add(path);
      }
    }
  }
  const tracked = (await git(root, ["ls-files", "--cached", "-z"], selectedEnvironment)).split("\0").filter(Boolean);
  const untracked = (await git(root, ["ls-files", "--others", "--exclude-standard", "-z"], selectedEnvironment)).split("\0").filter(Boolean);
  const selectedUntracked = untracked.filter((path) => [...roots].some((prefix) => path.startsWith(prefix)));
  const automatic: string[] = [];
  for (const path of AUTOMATIC_INPUTS) {
    try { await lstat(resolve(root, path)); automatic.push(path); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  }
  const paths = [...new Set([...tracked.filter((path) => !excluded(path)), ...selectedUntracked.filter((path) => !excluded(path)), ...automatic, ...required])].sort();
  const files: SourceFileIdentity[] = [];
  for (const raw of paths) {
    const path = normalized(raw);
    const abs = resolve(root, path);
    try {
      const before = await lstat(abs);
      const actual = await realpath(abs);
      if (!inside(root, actual)) throw new Error("symlink escapes the source root");
      if (excluded(relative(root, actual))) throw new Error("symlink resolves into excluded private/build/evidence material");
      let sha256: string;
      let mode: string;
      if (before.isSymbolicLink()) {
        const target = await lstat(actual);
        if (!target.isFile()) throw new Error("selected symlink must resolve to a regular file");
        sha256 = identityDigest({ link: await readlink(abs), content: await fileHash(actual) });
        mode = "120000";
      } else {
        if (!before.isFile()) throw new Error("selected input is not a regular file (submodules require an explicit material inventory)");
        sha256 = await fileHash(actual);
        mode = (before.mode & 0o111) !== 0 ? "100755" : "100644";
      }
      const after = await lstat(abs);
      if (before.ino !== after.ino || before.size !== after.size || before.mtimeMs !== after.mtimeMs || before.ctimeMs !== after.ctimeMs || before.mode !== after.mode) throw new Error("input changed while hashing");
      files.push({ path, mode, sha256 });
    } catch (error) {
      throw new Error(`source input ${path} cannot be captured: ${error instanceof Error ? error.message : String(error)}; restore the required input before qualifying`);
    }
  }
  const changes = (await git(root, ["diff-index", "--name-only", "-z", "HEAD", "--"], selectedEnvironment)).split("\0").filter(Boolean);
  const currentTools = await toolchain(root, selectedEnvironment);
  // Every delivered value is bound without exposing paths, addresses or selectors.
  const environment: Record<string, string> = {};
  for (const key of Object.keys(selectedEnvironment).sort()) environment[`${key}_sha256`] = identityDigest(selectedEnvironment[key]);
  if ((await git(root, ["rev-parse", "HEAD"], selectedEnvironment)).trim() !== commit) throw new Error("HEAD changed while capturing source; rerun after source mutations stop");
  return {
    root, commit, tree, files,
    source_digest: identityDigest(files),
    inventory_digest: identityDigest({ required: [...required].sort(), automatic_inputs: AUTOMATIC_INPUTS, untracked_roots: [...roots].sort(), excluded_directories: Object.keys(EXCLUDED_DIRECTORIES).sort(), excluded_roots: EXCLUDED_ROOTS, files: files.map(({ path, mode }) => ({ path, mode })) }),
    dirty: changes.some((path) => paths.includes(path)) || [...selectedUntracked, ...automatic, ...required].some((path) => !excluded(path) && !tracked.includes(path)),
    toolchain: currentTools,
    environment,
  };
}

export function isSourceIdentity(value: unknown): value is SourceIdentity {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const record = value as Record<string, unknown>;
  if (typeof record.root !== "string" || !isAbsolute(record.root) || typeof record.commit !== "string" || !/^[0-9a-f]{40,64}$/.test(record.commit) || typeof record.tree !== "string" || !/^[0-9a-f]{40,64}$/.test(record.tree) || typeof record.dirty !== "boolean" || typeof record.source_digest !== "string" || !HEX.test(record.source_digest) || typeof record.inventory_digest !== "string" || !HEX.test(record.inventory_digest)) return false;
  if (!Array.isArray(record.files) || record.files.length === 0 || !record.files.every((file: unknown) => {
    if (typeof file !== "object" || file === null || Array.isArray(file)) return false;
    const item = file as Record<string, unknown>;
    return typeof item.path === "string" && typeof item.mode === "string" && ["100644", "100755", "120000"].includes(item.mode) && typeof item.sha256 === "string" && HEX.test(item.sha256);
  })) return false;
  const tools = record.toolchain;
  const environment = record.environment;
  if (typeof environment !== "object" || environment === null || Array.isArray(environment) || Object.keys(environment).length === 0 || !Object.values(environment).every((value) => typeof value === "string")) return false;
  return typeof tools === "object" && tools !== null && !Array.isArray(tools) && Object.keys(tools).length > 0 && Object.values(tools).every((value) => typeof value === "string") && identityDigest(record.files) === record.source_digest;
}

export function sameSourceIdentity(left: SourceIdentity, right: SourceIdentity): boolean {
  return left.root === right.root && left.commit === right.commit && left.tree === right.tree && left.source_digest === right.source_digest && left.inventory_digest === right.inventory_digest && left.dirty === right.dirty && identityDigest(left.files) === identityDigest(right.files) && identityDigest(Object.entries(left.toolchain).sort()) === identityDigest(Object.entries(right.toolchain).sort()) && identityDigest(Object.entries(left.environment).sort()) === identityDigest(Object.entries(right.environment).sort());
}

/** Whitespace-separated literal argv only. Quotes, escapes and shell operators
 * are errors, not a pretend shell parser. Script shorthand supports Bun JS/TS. */
export function argvForGate(gate: string): string[] {
  if (!gate.trim() || /["'`\\$;&|<>\n\r\0]/.test(gate)) throw new Error("unsupported gate quoting or shell syntax; use whitespace-separated literal arguments and put complex commands in a checked-in script");
  const argv = gate.trim().split(/\s+/);
  return argv.length === 1 && /\.(?:ts|tsx|js|mjs|cjs)$/.test(argv[0]) ? ["bun", argv[0]] : argv;
}
