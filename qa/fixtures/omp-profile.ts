//! Isolated OMP (oh-my-pi) profile setup for the H1 native-harness smoke.
//!
//! OMP resolves auth, sessions, settings and its model catalog from a per-profile
//! agent directory (`~/.omp/profiles/<profile>/agent` when `--profile` is used,
//! verified against 18.8.9 via `omp --profile <p> config path`). Custom models are
//! registered by writing `models.yml` into that agent directory; a provider entry
//! with `discovery: { type: llama.cpp }` makes OMP enumerate a local llama.cpp
//! server's models (verified: `omp models ls` then lists them, and `omp acp`
//! selects the provider when it is the only one available in the profile).
//!
//! The helper never reads or writes the operator's own `~/.omp` tree: every path
//! it touches lives under the caller-supplied isolated HOME.
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

export interface OmpXdg {
  config: string;
  cache: string;
  data: string;
  state: string;
}

export interface LocalProvider {
  /** Provider key in models.yml (also the model selector namespace). */
  name: string;
  /** OpenAI-compatible transport, e.g. `openai-completions`. */
  api: string;
  /** Base URL of the local server, e.g. `http://127.0.0.1:8091`. */
  baseUrl: string;
  /** `auth: none` for a keyless local server; `apiKey` otherwise. */
  auth?: "none";
  /** Discovery block; `{ type: "llama.cpp" }` enumerates the server's own models. */
  discovery?: { type: string; injectV1?: boolean };
}

export interface OmpProfileOptions {
  ompBinary: string;
  home: string;
  xdg: OmpXdg;
  profile: string;
  providers: LocalProvider[];
}

export interface Probe {
  command: string;
  exit: number;
  stdout: string;
  stderr: string;
}

export interface OmpProfileResult {
  agentDir: string;
  modelsYml: string;
  probes: Probe[];
}

export function ompEnv(home: string, xdg: OmpXdg): Record<string, string> {
  return {
    PATH: process.env.PATH ?? "/usr/local/bin:/usr/bin:/bin",
    LANG: process.env.LANG ?? "C.UTF-8",
    LC_ALL: process.env.LC_ALL ?? "C.UTF-8",
    HOME: home,
    XDG_CONFIG_HOME: xdg.config,
    XDG_CACHE_HOME: xdg.cache,
    XDG_DATA_HOME: xdg.data,
    XDG_STATE_HOME: xdg.state,
  };
}

/** One bounded command capture; returns the exact command and its real output. */
export async function runCapture(
  binary: string,
  args: string[],
  env: Record<string, string>,
  cwd: string,
  timeoutMs = 60_000,
): Promise<Probe> {
  const child = Bun.spawn([binary, ...args], { env, cwd, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGKILL"), timeoutMs);
  const [stdout, stderr, exit] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  clearTimeout(timer);
  return { command: `${binary} ${args.join(" ")}`, exit, stdout, stderr };
}

function renderModelsYml(providers: LocalProvider[]): string {
  const lines: string[] = ["providers:"];
  for (const provider of providers) {
    lines.push(`  ${provider.name}:`);
    lines.push(`    api: ${provider.api}`);
    lines.push(`    baseUrl: ${provider.baseUrl}`);
    if (provider.auth === "none") lines.push(`    auth: none`);
    if (provider.discovery) {
      lines.push(`    discovery:`);
      lines.push(`      type: ${provider.discovery.type}`);
      if (provider.discovery.injectV1 !== undefined) lines.push(`      injectV1: ${String(provider.discovery.injectV1)}`);
    }
  }
  return `${lines.join("\n")}\n`;
}

/**
 * Write an executable wrapper that launches the real OMP with a scoped toolset
 * and no ambient skills/rules/LSP. The `OmpAdapter` spawns a fixed
 * `--profile <p> acp`, so the only way to cap OMP's engine prompt (which is
 * otherwise ~7300 tokens of tool schemas) is through the `binary` option: the
 * wrapper passes the extra global flags ahead of the adapter's own args. OMP's
 * ACP `session/prompt` produces no frame until the model answers, and the
 * adapter abandons the turn after 120 s of silence, so a slow CPU-only local
 * model needs this smaller prompt to answer in time.
 */
export async function writeScopedOmpWrapper(options: {
  ompBinary: string;
  dir: string;
  tools: string[];
  extraFlags?: string[];
}): Promise<string> {
  await mkdir(options.dir, { recursive: true });
  const wrapper = join(options.dir, "omp-scoped.sh");
  const flags = ["--no-skills", "--no-rules", "--no-lsp", `--tools=${options.tools.join(",")}`, ...(options.extraFlags ?? [])];
  const script = `#!/usr/bin/env bash\nexec ${JSON.stringify(options.ompBinary)} ${flags.map((flag) => JSON.stringify(flag)).join(" ")} "$@"\n`;
  await writeFile(wrapper, script, { mode: 0o755 });
  return wrapper;
}

/** Apply per-profile OMP settings via `omp --profile <p> config set`. */
export async function applyOmpSettings(ompBinary: string, home: string, xdg: OmpXdg, profile: string, settings: Record<string, string>): Promise<Probe[]> {
  const env = ompEnv(home, xdg);
  const probes: Probe[] = [];
  for (const [key, value] of Object.entries(settings)) {
    const probe = await runCapture(ompBinary, ["--profile", profile, "config", "set", key, value], env, home);
    probes.push(probe);
    if (probe.exit !== 0) throw new Error(`omp config set ${key}=${value} failed: ${probe.stderr.trim()}`);
  }
  return probes;
}

/** Write `models.yml` into the profile's agent dir and prove OMP can see the models. */
export async function prepareOmpProfile(options: OmpProfileOptions): Promise<OmpProfileResult> {
  const env = ompEnv(options.home, options.xdg);
  const probes: Probe[] = [];
  for (const dir of [options.home, options.xdg.config, options.xdg.cache, options.xdg.data, options.xdg.state]) await mkdir(dir, { recursive: true });
  const pathProbe = await runCapture(options.ompBinary, ["--profile", options.profile, "config", "path"], env, options.home);
  probes.push(pathProbe);
  const agentDir = pathProbe.stdout.trim();
  if (pathProbe.exit !== 0 || !agentDir) {
    throw new Error(`omp config path failed for profile ${options.profile}: exit ${pathProbe.exit} ${pathProbe.stderr.trim()}`);
  }
  await mkdir(agentDir, { recursive: true });
  const modelsYml = join(agentDir, "models.yml");
  await writeFile(modelsYml, renderModelsYml(options.providers));
  // Prove the provider is registered: `omp models <name> --json` must list a model.
  for (const provider of options.providers) {
    const probe = await runCapture(options.ompBinary, ["--profile", options.profile, "models", provider.name, "--json"], env, options.home, 90_000);
    probes.push(probe);
    if (probe.exit !== 0 || !probe.stdout.includes(provider.name)) {
      throw new Error(
        `models.yml provider "${provider.name}" did not register: exit ${probe.exit} stdout=${probe.stdout.slice(0, 300)} stderr=${probe.stderr.slice(0, 300)}`,
      );
    }
  }
  return { agentDir, modelsYml, probes };
}
