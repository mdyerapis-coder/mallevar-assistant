import type { JudgmentDomain, JudgmentState } from "../../../contracts/src/generated";
import { REQUIRED_CLASSES, type FixtureClass } from "./domains/types";

export interface ScoredObservation {
  row_id: string;
  class: FixtureClass;
  state: JudgmentState;
  choice: string | null;
  expected: string;
  score: number | null;
  runtime_identity_digest: string | null;
}
export interface ClassSelection {
  attempted: number;
  available: number;
  accepted: number;
  wrong: number;
  error_rate: number | null;
}
export interface SelectionSummary extends ClassSelection {
  threshold_logprob: number | null;
  per_class: Record<FixtureClass, ClassSelection>;
}
export interface CalibratorArtifact {
  version: 2;
  domain: JudgmentDomain;
  domain_version: string;
  prompt_digest: string;
  dataset_digest: string;
  calibration_digest: string;
  /** Null for unreviewed data: such an artifact cannot qualify a domain. */
  review_freeze_digest: string | null;
  runtime_identity_digest: string | null;
  feature: "raw_first_token_logprob";
  threshold_logprob: number | null;
  selection: SelectionSummary;
  artifact_digest: string;
}

function eligible(row: ScoredObservation): row is ScoredObservation & { score: number } {
  return row.state === "available" && typeof row.choice === "string" && row.choice.length > 0 && row.choice !== "ABSTAIN"
    && typeof row.score === "number" && Number.isFinite(row.score) && row.score <= 0;
}
// The same frozen selection contract must govern fitting receipts and held-out use.
function passesFrozenThreshold(row: ScoredObservation, threshold: number | null): boolean {
  return threshold !== null && Number.isFinite(threshold) && threshold <= 0 && eligible(row) && row.score >= threshold;
}
function emptyClass(): ClassSelection {
  return { attempted: 0, available: 0, accepted: 0, wrong: 0, error_rate: null };
}

/** Fit calibration only. Equal scores are indivisible; stop at the first unsafe full prefix. */
export function fitSelectiveThreshold(observations: readonly ScoredObservation[], targetError = 0.05): SelectionSummary {
  if (!Number.isFinite(targetError) || targetError < 0 || targetError > 1) throw new RangeError("Invalid selective error target");
  const ranked = observations.filter(eligible).sort((a, b) => b.score - a.score);
  let threshold: number | null = null;
  let prefixWrong = 0;
  for (let begin = 0; begin < ranked.length;) {
    const score = ranked[begin].score;
    let end = begin;
    while (end < ranked.length && ranked[end].score === score) {
      if (ranked[end].choice !== ranked[end].expected) prefixWrong++;
      end++;
    }
    if (prefixWrong / end > targetError) break;
    threshold = score;
    begin = end;
  }

  const per_class = { positive: emptyClass(), negative: emptyClass(), ambiguous: emptyClass(), hostile: emptyClass() };
  const result: SelectionSummary = { ...emptyClass(), threshold_logprob: threshold, per_class };
  // Recompute the published population, not the rejected prefix's working counters.
  for (const row of observations) {
    if (!REQUIRED_CLASSES.includes(row.class)) throw new TypeError("Unknown calibration class");
    const cls = per_class[row.class];
    result.attempted++;
    cls.attempted++;
    if (row.state === "available" && typeof row.choice === "string" && row.choice.length > 0 && row.choice !== "ABSTAIN") {
      result.available++; cls.available++;
    }
    if (!passesFrozenThreshold(row, threshold)) continue;
    result.accepted++;
    cls.accepted++;
    if (row.choice !== row.expected) { result.wrong++; cls.wrong++; }
  }
  result.error_rate = result.accepted > 0 ? result.wrong / result.accepted : null;
  for (const cls of Object.values(per_class)) cls.error_rate = cls.accepted > 0 ? cls.wrong / cls.accepted : null;
  return result;
}

/** Pure frozen predicate, not signature/provenance verification or permission to auto-select. */
export function applySelectiveThreshold(observation: ScoredObservation, artifact: CalibratorArtifact): { accepted: boolean; reason: string } {
  if (artifact.feature !== "raw_first_token_logprob") return { accepted: false, reason: "unsupported_feature" };
  const accepted = passesFrozenThreshold(observation, artifact.threshold_logprob);
  return { accepted, reason: accepted ? "selected" : "outside_frozen_selection" };
}
