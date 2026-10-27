import { expect, test } from "bun:test";
import {
  applySelectiveThreshold,
  fitSelectiveThreshold,
  type CalibratorArtifact,
  type ScoredObservation,
  type SelectionSummary,
} from "../src/jev/calibration";

function observation(id: string, overrides: Partial<ScoredObservation> = {}): ScoredObservation {
  return { row_id: id, class: "positive", state: "available", choice: "REVIEW", expected: "REVIEW",
    score: -0.1, runtime_identity_digest: "a".repeat(64), ...overrides };
}
function correctRows(count: number): ScoredObservation[] {
  return Array.from({ length: count }, (_, i) => observation(`correct-${i}`));
}
function artifact(selection: SelectionSummary): CalibratorArtifact {
  return { version: 2, domain: "task_intent", domain_version: "1", prompt_digest: "b".repeat(64),
    dataset_digest: "c".repeat(64), calibration_digest: "d".repeat(64), review_freeze_digest: null,
    runtime_identity_digest: "a".repeat(64), feature: "raw_first_token_logprob",
    threshold_logprob: selection.threshold_logprob, selection, artifact_digest: "e".repeat(64) };
}

test("equal_score_group_is_atomic", () => {
  const result = fitSelectiveThreshold([...correctRows(19),
    observation("wrong-1", { score: -0.2, choice: "WRITE" }),
    observation("wrong-2", { score: -0.2, choice: "WRITE" })]);
  expect(result.threshold_logprob).toBe(-0.1);
  expect(result.accepted).toBe(19);
  expect(result.wrong).toBe(0);
  expect(result.error_rate).toBe(0);
});

test("tie_order_does_not_change_selection", () => {
  const rows = [observation("right"), observation("wrong", { choice: "WRITE" })];
  for (const input of [rows, [...rows].reverse()]) {
    const result = fitSelectiveThreshold(input);
    expect(result.threshold_logprob).toBeNull();
    expect(result.accepted).toBe(0);
    expect(result.wrong).toBe(0);
    expect(result.error_rate).toBeNull();
  }
});

test("failed_prefix_is_not_final_population", () => {
  const result = fitSelectiveThreshold([...correctRows(19),
    observation("safe-error", { score: -0.2, choice: "WRITE" }),
    observation("rejected-error", { score: -0.3, choice: "WRITE" })]);
  expect(result.threshold_logprob).toBe(-0.2);
  expect(result.accepted).toBe(20);
  expect(result.wrong).toBe(1);
  expect(result.error_rate).toBe(0.05);
});

test("attempted_denominator_includes_unavailable", () => {
  const result = fitSelectiveThreshold([
    observation("correct"), observation("missing-score", { score: null }),
    observation("unavailable", { state: "unavailable", choice: null, score: null }),
    observation("invalid", { state: "invalid", choice: null, score: null }),
    observation("unqualified", { state: "unqualified", choice: null, score: null }),
    observation("abstained", { state: "abstain", choice: null, score: null }),
  ]);
  expect(result.attempted).toBe(6);
  expect(result.available).toBe(2);
  expect(result.accepted).toBe(1);
  expect(result.per_class.positive).toEqual({ attempted: 6, available: 2, accepted: 1, wrong: 0, error_rate: 0 });
});

test("per_class_counts_use_final_selection", () => {
  const result = fitSelectiveThreshold([...correctRows(19),
    observation("accepted-hostile-error", { class: "hostile", score: -0.2, choice: "WRITE" }),
    observation("rejected-hostile-error", { class: "hostile", score: -0.3, choice: "WRITE" })]);
  expect(result.per_class.hostile).toEqual({ attempted: 2, available: 2, accepted: 1, wrong: 1, error_rate: 1 });
  expect(result.per_class.positive).toEqual({ attempted: 19, available: 19, accepted: 19, wrong: 0, error_rate: 0 });
  expect(result.per_class.negative).toEqual({ attempted: 0, available: 0, accepted: 0, wrong: 0, error_rate: null });
});

test("null_threshold_accepts_nothing", () => {
  const rule = artifact(fitSelectiveThreshold([observation("wrong", { choice: "WRITE" })]));
  expect(applySelectiveThreshold(observation("later-correct", { score: 0 }), rule).accepted).toBe(false);
});

test("nonfinite_feature_is_rejected", () => {
  const rows = [observation("correct"), ...[NaN, Infinity, -Infinity, 0.1].map((score, i) =>
    observation(`invalid-feature-${i}`, { choice: "WRITE", score }))];
  const result = fitSelectiveThreshold(rows);
  expect(result.threshold_logprob).toBe(-0.1);
  expect(result.attempted).toBe(5);
  expect(result.accepted).toBe(1);
  expect(result.wrong).toBe(0);
  for (const row of rows.slice(1)) expect(applySelectiveThreshold(row, artifact(result)).accepted).toBe(false);
});

test("frozen_threshold_rejects_lower_scores_and_nonchoices", () => {
  const rule = artifact(fitSelectiveThreshold([observation("fit")]));
  expect(applySelectiveThreshold(observation("boundary"), rule).accepted).toBe(true);
  for (const overrides of [{ score: -0.100001 }, { score: null }, { choice: null }, { choice: "ABSTAIN" },
    { state: "invalid" as const }, { state: "unavailable" as const }, { state: "unqualified" as const }, { state: "abstain" as const }]) {
    expect(applySelectiveThreshold(observation("rejected", overrides), rule).accepted).toBe(false);
  }
});

test("fitting_stops_at_first_unsafe_group_even_if_later_rows_would_dilute_error", () => {
  const result = fitSelectiveThreshold([observation("unsafe-top", { choice: "WRITE", score: 0 }),
    ...correctRows(100)]);
  expect(result.threshold_logprob).toBeNull();
  expect(result.accepted).toBe(0);
});

test("empty_calibration_cannot_invent_an_error_rate", () => {
  const result = fitSelectiveThreshold([]);
  expect(result).toEqual({ threshold_logprob: null, attempted: 0, available: 0, accepted: 0, wrong: 0, error_rate: null,
    per_class: {
      positive: { attempted: 0, available: 0, accepted: 0, wrong: 0, error_rate: null },
      negative: { attempted: 0, available: 0, accepted: 0, wrong: 0, error_rate: null },
      ambiguous: { attempted: 0, available: 0, accepted: 0, wrong: 0, error_rate: null },
      hostile: { attempted: 0, available: 0, accepted: 0, wrong: 0, error_rate: null },
    } });
});

test("invalid_target_cannot_make_the_risk_rule_permissive", () => {
  for (const target of [-0.1, 1.1, NaN, Infinity]) expect(() => fitSelectiveThreshold(correctRows(1), target)).toThrow();
});
