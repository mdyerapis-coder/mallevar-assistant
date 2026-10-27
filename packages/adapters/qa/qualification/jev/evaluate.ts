#!/usr/bin/env bun
/**
 * J2 evaluation harness (qa/qualification/jev/evaluate.ts).
 *
 * Drives the REAL local JEV runtime (llama.cpp-backed facade at 127.0.0.1:8420)
 * against each domain's frozen holdout split and writes one canonical
 * EvaluationReportInput (packages/contracts/src/generated.ts) per domain to
 * stdout (one JSON line per domain). The report is exactly the DTO that
 * CoreCommand::judgment_qualify carries: the core re-derives the verdict with
 * judgments::qualification::gate() and ignores any state/reasons sent on the wire.
 *
 * Modes:
 *   --live       : hit the real facade+model (default).
 *   --offline    : no live model. Every row is reported `unqualified` because no
 *                  runtime identity was observed, so the gate blocks. Useful for
 *                  syntax/structural testing without a live model.
 *   --limit <n>  : cap rows per domain (smoke).
 *   --domain <d> : one domain or `all` (default all).
 *   --identity <digest> : override the pre-registered claimed identity digest
 *                  (mismatched-identity test only).
 *
 * The gate was calibrated against Qwen2.5-0.5B-Instruct-Q8_0.gguf
 * (claimed_identity = sha256 of its model_identity/path/build_info). A live
 * runtime that is NOT that exact identity fails CLOSED: the observed digest is
 * recorded as reported, so it cannot equal the claimed digest and every domain is
 * reported `blocked`. No green badge is ever produced for a mismatched identity;
 * no model is switched, and no scope is narrowed to fake availability.
 *
 * Only the HOLDOUT split is judged here, and only with the pre-registered claimed
 * identity: no threshold is fitted on these rows (calibrate.ts fits on the
 * calibration split).
 *
 * Exit status: 0 if every domain ended in a gate-decided state (enabled or
 * blocked with a documented reason); 2 if any domain is in an unexpected state or
 * the harness could not produce a report.
 */
import { stdout, stderr, exit } from "node:process";
import { JevClient } from "../../../src/jev/client";
import { lexicalJson, sha256 } from "../../../src/jev/identity";
import type { JudgmentInput, RuntimeIdentity } from "../../../src/jev/types";
import { DomainRegistry } from "../../../src/jev/registry";
import {
  binomUpper95,
  buildDataset,
  canAutoSelect,
  percentile,
  promptDigest,
  qualificationGate,
  REQUIRED_CLASSES,
  zeroClassMetrics,
  type Dataset,
  type DomainDefinition,
  type EvaluationReport as DomainEvaluationReport,
  type FixtureRow,
  type SplitName,
} from "../../../src/jev/domains/types";
import type {
  ClassMetrics,
  EvaluationReportInput,
  GateVerdict,
  LatencyMs,
  SplitDigests,
} from "../../../../contracts/src/generated";
import { decisionIdFor, offlineTransport, probeRuntimeIdentity } from "./registry";

// --- CLI -------------------------------------------------------------------

const args = process.argv.slice(2);
function flag(name: string): string | undefined {
  const i = args.indexOf(name);
  if (i < 0) return undefined;
  return args[i + 1];
}
const live = !args.includes("--offline");
const limitFlag = flag("--limit");
const limit = limitFlag === undefined ? undefined : parseInt(limitFlag, 10);
const domainFlag = flag("--domain") ?? "all";
const identityOverride = flag("--identity"); // for the mismatched-identity test

const ALL_DOMAINS = DomainRegistry.all();
const domainsToRun: DomainDefinition[] = domainFlag === "all"
  ? ALL_DOMAINS
  : ALL_DOMAINS.filter((d) => d.domain === domainFlag);
if (domainsToRun.length === 0) {
  stderr.write(`unknown judgment domain: ${domainFlag}\n`);
  exit(2);
}

/**
 * Digest that labels an unobserved runtime, so `observed_identity_digest` stays a
 * Digest (never null) while still failing the identity comparison.
 */
const UNOBSERVED_IDENTITY_DIGEST = sha256("ironbark-jev:runtime-identity-unobserved:v1");

// --- Per-domain evaluation --------------------------------------------------

interface RowOutcome {
  row: FixtureRow;
  state: "accepted" | "wrong" | "abstained" | "invalid" | "unavailable" | "unqualified";
  latency_ms: number;
  chosen: string | null;
}

async function runRow(client: JevClient, domain: DomainDefinition, row: FixtureRow, expected: string, claimedDigest: string | null): Promise<RowOutcome> {
  if (!claimedDigest) {
    // An unobserved identity cannot produce qualification evidence.
    return { row, state: "unqualified", latency_ms: 0, chosen: null };
  }
  const candidates = domain.rowCandidates ? domain.rowCandidates(row) : domain.candidates;
  const input: JudgmentInput = {
    // UUID: the core types DecisionId as a UUID; the id is stable per
    // (domain, row) so a replayed row keeps the same identity.
    decision_id: decisionIdFor(`${domain.domain}:${row.id}`),
    domain: domain.domain,
    domain_version: domain.version,
    criterion: domain.prompt,
    candidates: [...candidates],
    input_refs: [row.id],
    input_digest: row.content_digest,
    route_policy_digest: promptDigest(domain),
  };
  const t0 = Date.now();
  // A rejected judgment (invalid input or a synchronous caller abort) is an
  // unavailable observation, never a choice.
  const record = await client.judge(input, new AbortController().signal).catch(() => null);
  const latency_ms = Date.now() - t0;
  if (record === null) return { row, state: "unavailable", latency_ms, chosen: null };
  if (record.state === "available" && record.runtime_identity_digest !== claimedDigest)
    return { row, state: "unqualified", latency_ms, chosen: null };
  // `available`, `abstain`, `invalid`, `unavailable` and `unqualified` stay
  // distinct: no state collapses into another and none becomes a choice.
  switch (record.state) {
    case "abstain": return { row, state: "abstained", latency_ms, chosen: null };
    case "invalid": return { row, state: "invalid", latency_ms, chosen: null };
    case "unavailable": return { row, state: "unavailable", latency_ms, chosen: null };
    case "unqualified": return { row, state: "unqualified", latency_ms, chosen: null };
    case "available": {
      if (record.choice === undefined) return { row, state: "invalid", latency_ms, chosen: null };
      // An explicit ABSTAIN is an abstention, never an accepted wrong choice.
      if (record.choice === "ABSTAIN") return { row, state: "abstained", latency_ms, chosen: null };
      return { row, state: record.choice === expected ? "accepted" : "wrong", latency_ms, chosen: record.choice };
    }
  }
}

/**
 * Per-class holdout metrics. `accepted` counts every row the runtime answered
 * (correct and wrong) and `wrong` is the accepted-but-wrong subset, so
 * `wrong <= accepted` holds by construction. The other buckets are disjoint.
 */
function computeClassMetrics(rows: FixtureRow[], outcomes: RowOutcome[]): Record<string, ClassMetrics> {
  const byClass: Record<string, ClassMetrics> = {};
  for (const c of REQUIRED_CLASSES) byClass[c] = zeroClassMetrics();
  for (let i = 0; i < rows.length; i++) {
    const metrics = byClass[rows[i].class] ?? (byClass[rows[i].class] = zeroClassMetrics());
    metrics.total++;
    switch (outcomes[i].state) {
      case "accepted": metrics.accepted++; break;
      case "wrong": metrics.accepted++; metrics.wrong++; break;
      case "abstained": metrics.abstained++; break;
      case "invalid": metrics.invalid++; break;
      case "unavailable": metrics.unavailable++; break;
      case "unqualified": metrics.unqualified++; break;
    }
  }
  return byClass;
}

/** Latency summary over the real samples (ascending). No sample => zeros. */
function latencyMetrics(sorted: readonly number[]): LatencyMs {
  if (sorted.length === 0) return { mean: 0, p50: 0, p95: 0 };
  let total = 0;
  for (const value of sorted) total += value;
  return { mean: total / sorted.length, p50: percentile(sorted, 50), p95: percentile(sorted, 95) };
}

/**
 * Digest over a split's frozen ordered row ids, using the lexical shape the
 * domain builder uses (so `calibration` equals dataset.calibration_digest).
 */
function splitDigest(name: SplitName, rows: readonly FixtureRow[]): string {
  return sha256(lexicalJson({ [name]: rows.map((row) => row.id) }));
}

/**
 * Builds the canonical wire report for one domain. The deterministic gate is the
 * same code the unit tests exercise (`qualificationGate`), which is typed on the
 * domain module's report shape, so its view is derived from the canonical report
 * instead of the report being built twice.
 */
function buildReport(
  domain: DomainDefinition,
  holdout: FixtureRow[],
  outcomes: RowOutcome[],
  dataset: Dataset,
  observedIdentity: RuntimeIdentity | null,
  claimedDigest: string,
): { report: EvaluationReportInput; gate_input: DomainEvaluationReport } {
  const by_class = computeClassMetrics(holdout, outcomes);
  let accepted = 0, wrong = 0, abstained = 0, invalid = 0, unavailable = 0, unqualified = 0;
  for (const metrics of Object.values(by_class)) {
    accepted += metrics.accepted;
    wrong += metrics.wrong;
    abstained += metrics.abstained;
    invalid += metrics.invalid;
    unavailable += metrics.unavailable;
    unqualified += metrics.unqualified;
  }
  const size = holdout.length;
  const coverage = size > 0 ? accepted / size : 0;
  const upper_error_bound = binomUpper95(wrong, accepted);
  const latencies = outcomes.filter((o) => o.state !== "unqualified").map((o) => o.latency_ms).sort((a, b) => a - b);

  const observedModel = observedIdentity?.model_identity ?? "unobserved";
  const observedDigest = observedIdentity?.digest ?? UNOBSERVED_IDENTITY_DIGEST;
  const identity_match = observedIdentity !== null && observedIdentity.digest === claimedDigest;
  const prompt_digest = promptDigest(domain);
  const split_digests: SplitDigests = {
    development: splitDigest("development", dataset.splits.development),
    calibration: splitDigest("calibration", dataset.splits.calibration),
    holdout: splitDigest("holdout", dataset.splits.holdout),
  };
  const metrics = { size, accepted, wrong_accepted: wrong, abstained, invalid, unavailable, unqualified, coverage, upper_error_bound };

  const report: EvaluationReportInput = {
    domain: domain.domain,
    domain_version: domain.version,
    dataset_digest: dataset.dataset_digest,
    prompt_digest,
    split_digests,
    observed_identity_digest: observedDigest,
    observed_model_identity: observedModel,
    claimed_identity: claimedDigest,
    confidence_method: "none",
    holdout: { ...metrics, by_class },
    latency_ms: latencyMetrics(latencies),
    // `evaluated_at` is a signed wire decimal string; the core stamps its own
    // evaluated_at when the field is absent.
  };
  const gate_input: DomainEvaluationReport = {
    domain: domain.domain,
    domain_version: domain.version,
    prompt_digest,
    observed_model_identity: observedModel,
    observed_identity_digest: observedIdentity?.digest ?? null,
    claimed_identity_digest: claimedDigest,
    identity_match,
    dataset_digest: dataset.dataset_digest,
    calibration_digest: split_digests.calibration,
    identity: { model: observedModel, prompt_digest },
    holdout: { ...metrics, per_class: by_class },
    latency_ms: latencies,
    confidence_method: "none",
    confidence_method_description: "not applicable",
  };
  return { report, gate_input };
}

// --- Main -------------------------------------------------------------------

async function main(): Promise<void> {
  // Claimed identity is the Qwen2.5-0.5B the gate was calibrated against. In the
  // spec this is a fixed pre-registered digest; --identity overrides it for the
  // mismatched-identity test only.
  const claimedDigest = identityOverride ?? sha256(lexicalJson({
    model_identity: "Qwen2.5-0.5B-Instruct-Q8_0.gguf",
    model_path: "/models/qwen2.5-0.5b-instruct-q8_0.gguf",
    build_info: "llama.cpp b4495 cuda 12.9",
  }));

  let observedIdentity: RuntimeIdentity | null = null;
  if (live) {
    observedIdentity = await probeRuntimeIdentity((url, init) => fetch(url, init));
    if (!observedIdentity) {
      stderr.write("WARNING: runtime probe failed; treating as identity mismatch\n");
    }
  }

  let allOk = true;
  for (const domain of domainsToRun) {
    const dataset = await buildDataset(domain);
    const holdout = dataset.splits.holdout;
    const holdoutRows = limit ? holdout.slice(0, limit) : holdout;

    // Content is resolved by exact digest from the approved holdout rows only;
    // the client re-checks sha256(content) against input_digest itself.
    const texts = new Map<string, string>(holdoutRows.map((row): [string, string] => [row.content_digest, row.text]));
    const client = new JevClient({
      resolveInput: async (input) => {
        const text = texts.get(input.input_digest);
        if (text === undefined) throw new Error("judgment input is not an approved holdout row");
        return text;
      },
      transport: live ? ((url, init) => fetch(url, init)) : offlineTransport,
      max_in_flight: 1,
    });

    const identityMatch = observedIdentity !== null && observedIdentity.digest === claimedDigest;
    const outcomes: RowOutcome[] = [];
    for (const row of holdoutRows) {
      outcomes.push(await runRow(client, domain, row, row.expected, identityMatch ? claimedDigest : null));
    }

    const { report, gate_input } = buildReport(domain, holdoutRows, outcomes, dataset, observedIdentity, claimedDigest);
    const verdict: GateVerdict = qualificationGate(gate_input);
    // No fitted selection rule is loaded or applied by this evaluator.
    // Report identity fields are not a calibration artifact.
    const autoSelect = canAutoSelect(gate_input, null);

    // Output one JSON line per domain: the canonical report plus the locally
    // re-derived verdict (the core re-derives and persists its own).
    const out = { domain: domain.domain, state: verdict.state, reasons: verdict.reasons, auto_select: autoSelect, report,
      rows: outcomes.map(({ row, ...outcome }) => ({ id: row.id, class: row.class, expected: row.expected, ...outcome })) };
    stdout.write(JSON.stringify(out) + "\n");

    if (verdict.state !== "enabled" && verdict.state !== "blocked") allOk = false;
  }
  exit(allOk ? 0 : 2);
}

main().catch((e) => {
  stderr.write("FATAL: " + e.message + "\n");
  exit(2);
});
