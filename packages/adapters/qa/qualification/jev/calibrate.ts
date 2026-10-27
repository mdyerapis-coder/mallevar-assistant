#!/usr/bin/env bun
/**
 * J2 calibration fit (qa/qualification/jev/calibrate.ts).
 *
 * Fits a selective-risk abstention threshold on the CALIBRATION split for each
 * domain, using the REAL local JEV runtime. The threshold is never fitted on the
 * holdout split, and the receipt is bound to the exact dataset_digest and the
 * observed identity_digest — it is the J3 input and DOES NOT grant auto-select
 * authority by itself (canAutoSelect still requires a passing gate plus a
 * calibrator bound to this dataset and identity).
 *
 * Modes:
 *   --live       : hit the real facade+model (default).
 *   --offline    : no live model; every row is unqualified and the calibrator is
 *                  reported unavailable (no identity, no threshold).
 *   --domain <d> : one domain or `all` (default all).
 *
 * The fit uses the model's raw first-token log-probability. Equal-score groups
 * are indivisible; it keeps the widest safe prefix before the first group whose
 * empirical error exceeds 0.05. Counters describe the final frozen predicate,
 * including all attempts. No selected rows means null threshold and error.
 *
 * Output: one JSON line per domain — a diagnostic CalibratorArtifact. An
 * unreviewed dataset has no signed freeze and cannot qualify a domain.
 */
import { stdout, stderr, exit } from "node:process";
import { JevClient } from "../../../src/jev/client";
import { DomainRegistry } from "../../../src/jev/registry";
import type { JudgmentInput } from "../../../src/jev/types";
import {
  buildDataset,
  promptDigest,
  type FixtureClass,
  type DomainDefinition,
  type FixtureRow,
} from "../../../src/jev/domains/types";
import type { JudgmentState } from "../../../../contracts/src/generated";
import { decisionIdFor, offlineTransport, probeRuntimeIdentity } from "./registry";
import { fitSelectiveThreshold, type CalibratorArtifact, type ScoredObservation } from "../../../src/jev/calibration";
import { lexicalJson, sha256 } from "../../../src/jev/identity";
import { EvaluationRunIdentity } from "../../../src/jev/evaluation";


interface RowScore {
  score: number | null;
  chosen: string | null;
  state: JudgmentState;
  runtime_identity_digest: string | null;
  reason?: string;
}

async function runRowScore(client: JevClient, domain: DomainDefinition, row: FixtureRow): Promise<RowScore> {
  const candidates = domain.rowCandidates ? domain.rowCandidates(row) : domain.candidates;
  const input: JudgmentInput = {
    // UUID: the core types DecisionId as a UUID, and the id is stable per
    // (domain, row), so a replayed row keeps the same identity.
    decision_id: decisionIdFor(`${domain.domain}:${row.id}`),
    domain: domain.domain,
    domain_version: domain.version,
    criterion: domain.prompt,
    candidates: [...candidates],
    input_refs: [row.id],
    input_digest: row.content_digest,
    route_policy_digest: promptDigest(domain),
  };
  try {
    const record = await client.judge(input, new AbortController().signal);
    if (record.state !== "available" || record.choice === undefined) {
      return { score: null, chosen: null, state: record.state, runtime_identity_digest: record.runtime_identity_digest, reason: record.reason };
    }
    // The fit feature is the RAW first-token log probability only. A calibrated
    // selective-risk score is itself a fitted bound, so using it here would be
    // circular; any other score evidence is unknown, and an unknown score is
    // never replaced by an invented value.
    const score = record.score.type === "raw_token_log_prob" ? record.score.value : null;
    return { score, chosen: record.choice, state: record.state, runtime_identity_digest: record.runtime_identity_digest, reason: record.reason };
  } catch {
    return { score: null, chosen: null, state: "unavailable", runtime_identity_digest: null };
  }
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const domainIndex = args.indexOf("--domain");
  const domainFlag = domainIndex < 0 ? "all" : args[domainIndex + 1] ?? "all";
  const live = !args.includes("--offline");
  const allDomains = DomainRegistry.all();
  const domainsToRun = domainFlag === "all" ? allDomains : allDomains.filter(d => d.domain === domainFlag);
  if (domainsToRun.length === 0) {
    stderr.write(`unknown judgment domain: ${domainFlag}\n`);
    exit(2);
  }
  // One observed run spans every domain. Artifacts remain private in memory
  // until all row evidence and the final bounded metadata probe agree.
  const run = new EvaluationRunIdentity();
  const observedIdentity = live ? await probeRuntimeIdentity((url, init) => fetch(url, init)) : null;
  run.observe(observedIdentity?.digest ?? null, observedIdentity ? "available" : "unqualified");
  if (live && !observedIdentity) {
    stderr.write("WARNING: runtime probe failed; the calibrator binds to no identity\n");
  }
  const bodies: Omit<CalibratorArtifact, "artifact_digest" | "runtime_identity_digest">[] = [];

  for (const domain of domainsToRun) {
    const dataset = await buildDataset(domain);
    const calibration = dataset.splits.calibration;

    // Content is resolved by exact digest from the approved calibration rows
    // only; the client re-checks sha256(content) against input_digest itself.
    const texts = new Map<string, string>(calibration.map((row): [string, string] => [row.content_digest, row.text]));
    const client = new JevClient({
      resolveInput: async (input) => {
        const text = texts.get(input.input_digest);
        if (text === undefined) throw new Error("judgment input is not an approved calibration row");
        return text;
      },
      transport: live ? ((url, init) => fetch(url, init)) : offlineTransport,
      max_in_flight: 1,
    });

    const observations: ScoredObservation[] = [];
    for (const row of calibration) {
      const result = await runRowScore(client, domain, row);
      run.observe(result.runtime_identity_digest, result.state, result.reason);
      observations.push({
        row_id: row.id, class: row.class as FixtureClass, expected: row.expected,
        state: result.state, choice: result.chosen, score: result.score,
        runtime_identity_digest: result.runtime_identity_digest,
      });
    }
    const selection = fitSelectiveThreshold(observations);
    const body: Omit<CalibratorArtifact, "artifact_digest" | "runtime_identity_digest"> = {
      version: 2,
      domain: domain.domain,
      domain_version: domain.version,
      prompt_digest: promptDigest(domain),
      dataset_digest: dataset.dataset_digest,
      calibration_digest: dataset.calibration_digest,
      // Existing v1 rows have no independently signed review freeze.
      review_freeze_digest: null,
      feature: "raw_first_token_logprob",
      threshold_logprob: selection.threshold_logprob,
      selection,
    };
    bodies.push(body);
  }
  const finalIdentity = live ? await probeRuntimeIdentity((url, init) => fetch(url, init)) : null;
  run.observe(finalIdentity?.digest ?? null, finalIdentity ? "available" : "unqualified");
  const continuity = run.finish();
  for (const pending of bodies) {
    const body = { ...pending, runtime_identity_digest: continuity.identity_digest };
    const receipt: CalibratorArtifact = { ...body, artifact_digest: sha256(lexicalJson(body)) };
    stdout.write(JSON.stringify(receipt) + "\n");
  }
  if (live && !continuity.continuous) {
    stderr.write("runtime continuity failed: " + continuity.reasons.join(",") + "\n");
    process.exitCode = 1;
  }
}

if (import.meta.main) {
  main().catch((e) => {
    stderr.write("FATAL: " + e.message + "\n");
    exit(1);
  });
}
