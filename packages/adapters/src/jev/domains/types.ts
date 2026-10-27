// J2: shared types, frozen-split helpers and the deterministic enablement gate.
// The Rust core (src/judgments/qualification.rs) mirrors the gate and report
// shape exactly; field names and order must stay wire-compatible.
import { sha256, lexicalJson } from "../identity";
import { fileURLToPath } from "node:url";
import type { JudgmentDomain } from "../../../../contracts/src/generated";
export type { JudgmentDomain } from "../../../../contracts/src/generated";
/** Curated class vocabulary. Every domain must represent all four in holdout. */
export const REQUIRED_CLASSES = ["positive", "negative", "ambiguous", "hostile"] as const;
export type FixtureClass = (typeof REQUIRED_CLASSES)[number];

export const EXPECTED_ROWS = 1000;
export const MIN_HOLDOUT = 200;
/** Engineering acceptance values chosen before any evaluation, not fitted. */
export const MIN_ACCEPTANCE_COVERAGE = 0.3;
export const MAX_UPPER_ERROR_BOUND = 0.05;

export type SplitName = "development" | "calibration" | "holdout";
export type RowClass = FixtureClass | "calibration";

export interface FixtureRow {
  id: string;
  domain: string;
  version: string;
  class: RowClass;
  text: string;
  expected: string;
  /** Curation metadata; the domain label rule re-derives `expected` from it. */
  meta: Record<string, number>;
  content_digest: string;
  /** Stored partition tag (re-derived+verified, never trusted blindly). */
  split: SplitName;
}

export interface DomainDefinition {
  domain: JudgmentDomain;
  version: string;
  /** Frozen decision criterion embedded in the J1 exact-code prompt. */
  prompt: string;
  /** Fixed label set, or use rowCandidates for per-row (policy-filtered) sets. */
  candidates: readonly string[];
  rowCandidates?: (row: FixtureRow) => readonly string[];
  /** Validates that a reviewed expected label is a canonical candidate or ABSTAIN. */
  validateExpected: (label: string, rowCandidates: readonly string[]) => void;
  requiredClasses: readonly FixtureClass[];
  /** Frozen fixture sources (relative paths from IRONBARK_ROOT, or URLs). */
  fixtures: readonly string[];
}

/** Shared candidate-membership validation: accepts ABSTAIN unconditionally and any
 * exact canonical label in the supplied row candidate set. Rejects every other
 * string so metadata-derived or invented labels can never be treated as gold. */
export function validateExpected(label: string, rowCandidates: readonly string[]): void {
  if (label === "ABSTAIN") return;
  if (rowCandidates.includes(label)) return;
  throw new Error(`invalid expected label: ${label} not in candidate set`);
}

export interface Splits {
  development: FixtureRow[];
  calibration: FixtureRow[];
  holdout: FixtureRow[];
}

export interface Dataset {
  domain: DomainDefinition;
  splits: Splits;
  /** sha256 over the frozen domain prompt+version + ordered holdout ids. */
  dataset_digest: string;
  /** sha256 over the ordered calibration ids (the fit partition). */
  calibration_digest: string;
}

/** Per-class holdout accuracy metrics. */
export interface ClassMetrics {
  total: number;
  accepted: number;
  wrong: number;
  abstained: number;
  invalid: number;
  unavailable: number;
  unqualified: number;
}

/** Aggregate holdout metrics for one domain. */
export interface HoldoutMetrics {
  size: number;
  accepted: number;
  /** Accepted-but-wrong decisions. */
  wrong_accepted: number;
  abstained: number;
  invalid: number;
  unavailable: number;
  unqualified: number;
  /** accepted / size (guard: 0 when size==0). */
  coverage: number;
  /** One-sided 95% upper bound on the wrong-decision rate among accepted choices. */
  upper_error_bound: number;
  per_class: Record<string, ClassMetrics>;
}

/** Latency sample (ms, end-to-end) collected during a real evaluation. */
export type LatencyMs = number;

export interface EvaluationReport {
  domain: JudgmentDomain;
  domain_version: string;
  prompt_digest: string;
  observed_model_identity: string;
  observed_identity_digest: string | null;
  claimed_identity_digest: string;
  identity_match: boolean;
  dataset_digest: string;
  calibration_digest: string;
  /** The exact model+prompt identity the report was produced under. */
  identity: { model: string; prompt_digest: string };
  holdout: HoldoutMetrics;
  latency_ms: LatencyMs[];
  confidence_method: string;
  confidence_method_description: string;
}

/** EvaluationReport as emitted to core (the input core persists). */
export type EvaluationReportInput = Omit<EvaluationReport, "confidence_method_description">;

/** State of a domain's qualification (mirrors Rust QualificationState). */
export type QualificationState = "enabled" | "blocked";

export interface GateVerdict {
  state: QualificationState;
  reasons: string[];
}

/** Lexical prompt digest: bound to domain, frozen version and frozen criterion. */
export function promptDigest(domain: DomainDefinition): string {
  return sha256(lexicalJson({ domain: domain.domain, version: domain.version, prompt: domain.prompt }));
}

/**
 * Deterministic frozen 60/20/20 partition: rows are ranked by sha256(id) and
 * the rank order is cut at exactly 60%/80%. The cut is reproducible from the
 * frozen ids alone, so a loader can re-derive it and detect tampering.
 */
export function assignSplits(ids: readonly string[]): Record<string, SplitName> {
  const ranked = [...ids];
  ranked.sort((a, b) => {
    const da = sha256(a);
    const db = sha256(b);
    if (da < db) return -1;
    if (da > db) return 1;
    return 0;
  });
  const n = ranked.length;
  const devEnd = Math.floor((n * 60) / 100);
  const calEnd = Math.floor((n * 80) / 100);
  const out: Record<string, SplitName> = {};
  for (let i = 0; i < n; i++) out[ranked[i]] = i < devEnd ? "development" : i < calEnd ? "calibration" : "holdout";
  return out;
}

// --- Binomial helpers (exact Clopper-Pearson bounds, log-space) -------------


/** Exact binomial CDF P(X <= x) for small n, accumulated in log space. */
export function binomCdf(x: number, n: number, p: number): number {
  if (x < 0) return 0;
  if (x >= n || p <= 0) return 1;
  if (p >= 1) return 0;
  const logFailure = Math.log1p(-p);
  let logTerm = n * logFailure;
  let logCdf = -Infinity;
  for (let k = 0; k <= x; k++) {
    if (k > 0) logTerm += Math.log(n - k + 1) - Math.log(k) + Math.log(p) - logFailure;
    const max = Math.max(logCdf, logTerm);
    logCdf = max + Math.log1p(Math.exp(Math.min(logCdf, logTerm) - max));
  }
  return Math.min(1, Math.exp(logCdf));
}

/**
 * One-sided 95% Clopper-Pearson upper bound on the binomial failure rate for
 * `wrong` failures out of `accepted` accepted choices. With zero accepted
 * choices the bound is the conservative maximum (1), never an invented number.
 */
export function binomUpper95(wrong: number, accepted: number): number {
  if (accepted <= 0) return 1;
  if (wrong >= accepted) return 1;
  // CP upper bound: largest p s.t. P(X <= wrong | p) >= 0.05  (one-sided 95%)
  // i.e., find p such that binomCdf(wrong, accepted, p) = 0.05
  if (wrong === 0) return 1 - Math.pow(0.05, 1 / accepted);
  let lo = 0, hi = 1;
  for (let _ = 0; _ < 100; _++) {
    const mid = (lo + hi) / 2;
    const cdf = binomCdf(wrong, accepted, mid);
    if (cdf > 0.05) lo = mid; else hi = mid;
  }
  return hi;
}

export function zeroClassMetrics(): ClassMetrics {
  return { total: 0, accepted: 0, wrong: 0, abstained: 0, invalid: 0, unavailable: 0, unqualified: 0 };
}

/** Deterministic enablement gate, re-derived from the report's raw counts. */
export function qualificationGate(report: EvaluationReport): GateVerdict {
  const r = report.holdout;
  const reasons: string[] = [];
  if (r.size < MIN_HOLDOUT) reasons.push("insufficient_holdout");
  for (const c of REQUIRED_CLASSES) {
    const m = r.per_class[c];
    if (!m || m.total === 0) reasons.push("class_missing:" + c);
  }
  const coverageOk = r.size > 0 && r.coverage >= MIN_ACCEPTANCE_COVERAGE;
  if (!coverageOk) reasons.push("insufficient_coverage");
  const boundOk = r.upper_error_bound <= MAX_UPPER_ERROR_BOUND;
  if (!boundOk) reasons.push("upper_error_bound_exceeded");
  if (!report.identity_match) reasons.push("identity_mismatch");
  const state: QualificationState = reasons.length === 0 ? "enabled" : "blocked";
  return { state, reasons };
}

/**
 * Auto-selection requires a passing gate AND a calibrated selective-risk
 * signal bound to this exact dataset and identity. Raw first-token log
 * probabilities or missing scores never grant auto-selection.
 */
export function canAutoSelect(
  report: EvaluationReport,
  calibrator: { dataset_digest: string; identity_digest: string } | null,
): boolean {
  const verdict = qualificationGate(report);
  if (verdict.state !== "enabled") return false;
  if (!calibrator) return false;
  const identityOk = report.observed_identity_digest === calibrator.identity_digest;
  const datasetOk = report.dataset_digest === calibrator.dataset_digest;
  return identityOk && datasetOk;
}

export function percentile(sorted: readonly number[], q: number): number {
  if (sorted.length === 0) return 0;
  const i = (q / 100) * (sorted.length - 1);
  const lo = Math.floor(i);
  const hi = Math.ceil(i);
  if (lo === hi) return sorted[lo];
  const frac = i - lo;
  return sorted[lo] * (1 - frac) + sorted[hi] * frac;
}

export function loadJsonl(text: string): unknown[] {
  const out: unknown[] = [];
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    out.push(JSON.parse(trimmed));
  }
  return out;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Frozen-fixture validation. A tampered payload (content_digest != sha256(text))
 * fails fast. Splits are re-derived from row ids, never trusted from the row.
 */
export function validateFixtureRows(rows: unknown[]): FixtureRow[] {
  const out: FixtureRow[] = [];
  const seenContent = new Set<string>();
  const seenNormalized = new Set<string>();
  const seenIds = new Set<string>();
  for (const r of rows) {
    if (!isPlainObject(r)) throw new Error("fixture row is not an object");
    const row = r as Record<string, unknown>;
    const id = typeof row.id === "string" ? row.id : "";
    const domain = typeof row.domain === "string" ? row.domain : "";
    const version = typeof row.version === "string" ? row.version : "";
    const text = typeof row.text === "string" ? row.text : "";
    const expected = typeof row.expected === "string" ? row.expected : "";
    const meta = isPlainObject(row.meta) ? row.meta : null;
    const content_digest = typeof row.content_digest === "string" ? row.content_digest : "";
    const split = row.split as SplitName | undefined;
    if (!id || !domain || !version || !expected || !meta || !content_digest || !split) {
      throw new Error(`fixture row ${id || "?"} has missing fields`);
    }
    if (!["development", "calibration", "holdout"].includes(split)) {
      throw new Error(`fixture row ${id} has unknown split ${split}`);
    }
    if (content_digest !== sha256(text)) {
      throw new Error(`fixture row ${id} content_digest mismatch (tampered payload)`);
    }
    if (!REQUIRED_CLASSES.includes(row.class as FixtureClass)) {
      throw new Error(`fixture row ${id} has unknown class`);
    }
    if (seenIds.has(id)) throw new Error(`fixture row ${id} has duplicate ID`);
    if (seenContent.has(content_digest)) throw new Error(`fixture row ${id} has duplicate content; repeated observations cannot qualify a domain`);
    if (seenNormalized.has(text.trim())) throw new Error(`fixture row ${id} has duplicate normalized content; repeated observations cannot qualify a domain`);
    seenIds.add(id);
    seenContent.add(content_digest);
    seenNormalized.add(text.trim());
    out.push({
      id, domain, version,
      class: row.class as RowClass,
      text, expected,
      meta: meta as Record<string, number>,
      content_digest,
      split,
    });
  }
  return out;
}

/**
 * Dataset = frozen domain definition plus split+validate the frozen rows.
 * The split is re-derived from row ids (assignSplits) and verified against the
 * stored per-row split tag — a fixture whose partition was edited is rejected.
 */
export async function buildDataset(domain: DomainDefinition): Promise<Dataset> {
  const root = process.env.IRONBARK_ROOT ?? process.cwd();
  const raw: unknown[] = [];
  for (const f of domain.fixtures) {
    const text = await Bun.file(resolveFixture(root, f)).text();
    raw.push(...loadJsonl(text));
  }
  const all = validateFixtureRows(raw);
  const ranked = assignSplits(all.map((r) => r.id));
  for (const row of all) {
    if (ranked[row.id] !== row.split) {
      throw new Error(`fixture row ${row.id} split mismatch (tampered partition)`);
    }
  }
  const development = all.filter((r) => r.split === "development");
  const calibration = all.filter((r) => r.split === "calibration");
  const holdout = all.filter((r) => r.split === "holdout");
  // Validate each expected label against the domain candidate set (or per-row
  // policy-filtered set). Expected labels are curated assertions, not metadata-
  // derived re-derivations — labelRule(meta) is no longer gold-label authority.
  for (const row of all) {
    const rowCandidates = domain.rowCandidates ? domain.rowCandidates(row) : domain.candidates;
    domain.validateExpected(row.expected, rowCandidates);
  }
  const splits: Splits = { development, calibration, holdout };
  return {
    domain,
    splits,
    dataset_digest: sha256(lexicalJson({ domain: domain.domain, version: domain.version, rows: all })),
    calibration_digest: sha256(lexicalJson({ calibration: calibration.map((r) => r.id) })),
  };
}

function resolveFixture(root: string, f: string): string {
  if (f.startsWith("file://")) return fileURLToPath(f);
  if (f.startsWith("http://") || f.startsWith("https://")) return f;
  return root.endsWith("/") ? root + f : root + "/" + f;
}