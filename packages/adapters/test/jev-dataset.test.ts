import { describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { assignSplits, buildDataset } from "../src/jev/domains/types";
import type { DomainDefinition, FixtureRow } from "../src/jev/domains/types";
import { handoffDomain } from "../src/jev/domains/handoff";
import { evidenceDomain } from "../src/jev/domains/evidence";
import { intentDomain } from "../src/jev/domains/intent";
import { relevanceDomain } from "../src/jev/domains/relevance";
import { routesDomain } from "../src/jev/domains/routes";
import { sha256 } from "../src/jev/identity";
import { semanticScenarios } from "./fixtures/jev-reviewed-cases";

/** No fake VerifiedReviewFreeze: unresolved review/family contracts are in the report. */
test("C1 exposes the planned dataset boundary (intentional missing-module RED)", async () => {
  // Intentional module-loading-boundary test: a static import of the absent
  // C1 module would prevent the independent legacy-loader diagnostics running.
  const dataset = await import("../src/jev/dataset");
  expect(typeof dataset.buildReviewedDataset).toBe("function");
  expect(typeof dataset.familySplit).toBe("function");
});

/**
 * Exercise the existing aggregate file-loader boundary, not a mock or fake
 * review. These two-row inputs are diagnostic v1 data, never a gold dataset.
 * Positive controls make missing files, small-set rejection or an unrelated
 * future freeze failure visible rather than falsely satisfying a negative.
 */
function legacyRows(): FixtureRow[] {
  const ids = ["c1-diagnostic-migration", "c1-diagnostic-attachment"];
  const splits = assignSplits(ids);
  return ["handoff-verified-resume-context", "handoff-recovery-context"].map((scenarioId, index) => {
    const scenario = semanticScenarios.find(s => s.id === scenarioId);
    if (!scenario) throw new Error(`Missing authored diagnostic scenario ${scenarioId}`);
    return {
      id: ids[index], domain: scenario.domain, version: handoffDomain.version,
      class: scenario.class, text: scenario.text, expected: scenario.proposed_expected,
      // Compatibility with v1's current COMPLETE default only. This metadata
      // is NOT the semantic oracle and must not be carried into reviewed v2.
      meta: {}, content_digest: sha256(scenario.text), split: splits[ids[index]],
    };
  });
}

async function loadSeparateFiles(rows: readonly FixtureRow[]) {
  const directory = await mkdtemp(join(tmpdir(), "ironbark-c1-dataset-"));
  try {
    const fixtures: string[] = [];
    for (const [index, row] of rows.entries()) {
      const file = join(directory, `source-${index}.jsonl`);
      await Bun.write(file, JSON.stringify(row) + "\n");
      fixtures.push(pathToFileURL(file).href);
    }
    return await buildDataset({ ...handoffDomain, fixtures });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

async function assertLegacyPositiveControl(rows: readonly FixtureRow[]) {
  const dataset = await loadSeparateFiles(rows);
  const loaded = Object.values(dataset.splits).flat();
  expect(loaded.map(row => row.id).sort()).toEqual(rows.map(row => row.id).sort());
}

describe("existing aggregate loader structural diagnostics, not review acceptance", () => {
  test("rejects duplicate IDs across separate source files with different valid text digests", async () => {
    const rows = legacyRows();
    await assertLegacyPositiveControl(rows);
    await expect(loadSeparateFiles([rows[0], { ...rows[1], id: rows[0].id }])).rejects.toThrow();
  });

  test("rejects exact repeated content across separate source files despite distinct IDs", async () => {
    const rows = legacyRows();
    await assertLegacyPositiveControl(rows);
    await expect(loadSeparateFiles([
      rows[0], { ...rows[1], text: rows[0].text, content_digest: rows[0].content_digest },
    ])).rejects.toThrow();
  });

  test("rejects boundary-whitespace copies across source files after normalized-content comparison", async () => {
    const rows = legacyRows();
    await assertLegacyPositiveControl(rows);
    const whitespaceCopy = ` \r\n${rows[0].text}\t\n`;
    // Each raw digest is correct and different; only normalized-content
    // detection can reject this pair. No invented Unicode/case folding rule.
    await expect(loadSeparateFiles([
      rows[0], { ...rows[1], text: whitespaceCopy, content_digest: sha256(whitespaceCopy) },
    ])).rejects.toThrow();
  });

  test("rejects edited text with its old content digest before any review or inference", async () => {
    const rows = legacyRows();
    await assertLegacyPositiveControl(rows);
    await expect(loadSeparateFiles([
      rows[0], { ...rows[1], text: rows[1].text + " Correction: checksum report is unavailable." },
    ])).rejects.toThrow();
  });
});

/** Runtime narrowing of the planned exact method, not a new production DTO. */
function candidateValidator(domain: DomainDefinition) {
  const validate = Reflect.get(domain, "validateExpected");
  expect(typeof validate).toBe("function");
  return (label: string, candidates: readonly string[]) => Reflect.apply(validate, domain, [label, candidates]);
}

const domains = [handoffDomain, evidenceDomain, intentDomain, relevanceDomain, routesDomain];

describe("planned candidate validation only; NOT a semantic truth oracle", () => {
  for (const domain of domains) {
    test(`${domain.domain} accepts every exact candidate and explicit ABSTAIN`, () => {
      const validate = candidateValidator(domain);
      for (const label of domain.candidates) expect(() => validate(label, domain.candidates)).not.toThrow();
      expect(() => validate("ABSTAIN", domain.candidates)).not.toThrow();
    });

    test(`${domain.domain} rejects invented labels instead of trusting reviewer metadata`, () => {
      const validate = candidateValidator(domain);
      // Require a working positive control in this very test, before rejection.
      expect(() => validate(domain.candidates[0], domain.candidates)).not.toThrow();
      expect(() => validate("NOT_A_CANONICAL_LABEL", domain.candidates)).toThrow();
    });
  }

  test.each(["COMP", "COMPLETE!", "complete", "INCOMPLETE suffix"])(
    "handoff rejects non-exact candidate %s despite shared prefixes", label => {
      const validate = candidateValidator(handoffDomain);
      expect(() => validate("COMPLETE", handoffDomain.candidates)).not.toThrow();
      expect(() => validate("INCOMPLETE", handoffDomain.candidates)).not.toThrow();
      expect(() => validate(label, handoffDomain.candidates)).toThrow();
    },
  );

  test.each(["paid-tier", "external-public", "local-gpu-free"])(
    "routes reject globally known but row-policy-excluded %s", forbidden => {
      const validate = candidateValidator(routesDomain);
      const allowed = ["local-cpu-free", "owner-manual"];
      expect(() => validate("local-cpu-free", allowed)).not.toThrow();
      expect(() => validate("owner-manual", allowed)).not.toThrow();
      expect(() => validate("ABSTAIN", allowed)).not.toThrow();
      expect(() => validate(forbidden, allowed)).toThrow();
    },
  );

  test("an empty allowed route set permits abstention but no route", () => {
    const validate = candidateValidator(routesDomain);
    expect(() => validate("ABSTAIN", [])).not.toThrow();
    for (const route of routesDomain.candidates) expect(() => validate(route, [])).toThrow();
  });
});
