// C1: reviewed dataset assembly — buildReviewedDataset and familySplit.
// This module binds independently reviewed rows, a frozen family-disjoint split
// and a VerifiedReviewFreeze. The freeze itself is nonforgeable from public data
// (see C4/H2); when absent the dataset is structurally valid but not qualified
// for inference.
import { sha256, lexicalJson } from "./identity";
import type { DomainDefinition, FixtureClass, SplitName } from "./domains/types";

/** A single reviewed fixture row. No classifier access to label/rubric/review metadata. */
export interface ReviewedFixtureRow {
  id: string;
  domain: string;
  version: string;
  class: FixtureClass;
  text: string;
  expected: string;
  content_digest: string;
  family_id: string;
  split: SplitName;
  review_digest: string;
}

/** A semantic family: rows that are paraphrases and must share one split. */
export interface ScenarioFamily {
  family_id: string;
  row_ids: readonly string[];
  class: FixtureClass;
}

/** Frozen family-disjoint partition: each family goes entirely to one split. */
export interface SplitAssignment {
  development: readonly string[];
  calibration: readonly string[];
  holdout: readonly string[];
}

/** Signed freeze statement binding six exact digests; excluding its own digest. */
export interface FreezeStatement {
  benchmark_digest: string;
  dataset_digest: string;
  prompt_digest: string;
  rubric_digest: string;
  family_split_digest: string;
  review_ledger_digest: string;
  statement_artifact: string;
  statement_digest: string;
}

/** A freeze verified by an H-verified SSH signature over an owner key. */
export interface VerifiedReviewFreeze {
  freeze_digest: string;
  statement: FreezeStatement;
  signer_fingerprint: string;
  signature: string;
}

/** Manifest version 2: the dataset envelope reviewed rows bind to. */
export interface DatasetManifest {
  version: 2;
  domain: string;
  domain_version: string;
  prompt_digest: string;
  rows_digest: string;
  family_split_digest: string;
  review_freeze_digest: string | null;
}

/** A assembled reviewed dataset: manifest, rows, family split and logical digest. */
export interface Dataset {
  manifest: DatasetManifest;
  rows: readonly ReviewedFixtureRow[];
  family_split: SplitAssignment;
  /** sha256 over canonical domain/version/prompt plus ordered reviewed content,
   * labels, families, splits and review-ledger references — excluding the freeze pointer. */
  dataset_digest: string;
}

/**
 * Deterministic family-disjoint 60/20/20 split. Families are ordered by a
 * seeded hash of family_id (input-order independent) and then assigned to
 * splits preserving the 60%/80% cumulative-weight cuts. Each family lands
 * entirely in one split.
 */
export function familySplit(
  families: readonly ScenarioFamily[],
  seedDigest: string,
): SplitAssignment {
  const sorted = [...families].sort((a, b) => {
    const ha = sha256(seedDigest + a.family_id);
    const hb = sha256(seedDigest + b.family_id);
    if (ha < hb) return -1;
    if (ha > hb) return 1;
    return 0;
  });

  const totalWeight = sorted.reduce((sum, f) => sum + f.row_ids.length, 0);
  const devEnd = Math.ceil((totalWeight * 60) / 100);
  const calEnd = Math.ceil((totalWeight * 80) / 100);

  const assignment = new Map<string, SplitName>();
  let cumulative = 0;
  for (const family of sorted) {
    cumulative += family.row_ids.length;
    const split: SplitName =
      cumulative <= devEnd ? "development" : cumulative <= calEnd ? "calibration" : "holdout";
    assignment.set(family.family_id, split);
  }

  return {
    development: sorted.filter((f) => assignment.get(f.family_id) === "development").map((f) => f.family_id),
    calibration: sorted.filter((f) => assignment.get(f.family_id) === "calibration").map((f) => f.family_id),
    holdout: sorted.filter((f) => assignment.get(f.family_id) === "holdout").map((f) => f.family_id),
  };
}

/**
 * Assemble a reviewed dataset from independently reviewed rows and a verified
 * freeze. Validates row integrity (digest, duplicates, label membership),
 * freezes the family-disjoint split before inference, and computes the
 * manifest and logical dataset digest excluding the freeze pointer.
 */
export function buildReviewedDataset(
  domain: DomainDefinition,
  rows: readonly ReviewedFixtureRow[],
  freeze: VerifiedReviewFreeze | null,
): Dataset {
  // Reviewed rows deliberately omit the raw `meta` curation field (see
  // ReviewedFixtureRow), so per-row candidate derivation (DomainDefinition
  // .rowCandidates) cannot be re-run here. The reviewed assembly therefore
  // supports fixed-candidate domains only; per-row domains assemble through
  // the raw buildDataset path, which holds the full FixtureRow.
  if (domain.rowCandidates) {
    throw new Error("reviewed dataset assembly requires a fixed-candidate domain; per-row domains use buildDataset");
  }
  // --- Row integrity validation ---
  const seenIds = new Set<string>();
  const seenContent = new Set<string>();
  const seenNormalized = new Set<string>();
  for (const row of rows) {
    if (seenIds.has(row.id)) {
      throw new Error(`duplicate row ID: ${row.id}`);
    }
    if (row.content_digest !== sha256(row.text)) {
      throw new Error(`content digest mismatch: ${row.id}`);
    }
    if (seenContent.has(row.content_digest)) {
      throw new Error(`duplicate content digest: ${row.id}`);
    }
    if (seenNormalized.has(row.text.trim())) {
      throw new Error(`duplicate normalized content: ${row.id}`);
    }
    const rowCandidates = domain.candidates;
    domain.validateExpected(row.expected, rowCandidates);
    seenIds.add(row.id);
    seenContent.add(row.content_digest);
    seenNormalized.add(row.text.trim());
  }

  // --- Build scenario families ---
  const familyMap = new Map<string, ScenarioFamily>();
  for (const row of rows) {
    const existing = familyMap.get(row.family_id);
    if (existing) {
      existing.row_ids = [...existing.row_ids, row.id];
    } else {
      familyMap.set(row.family_id, {
        family_id: row.family_id,
        row_ids: [row.id],
        class: row.class,
      });
    }
  }
  const families = Array.from(familyMap.values());

  // --- Family-disjoint split (before inference) ---
  const seedDigest = sha256(
    lexicalJson({ domain: domain.domain, version: domain.version, rows: rows.map((r) => r.content_digest) }),
  );
  const family_split = familySplit(families, seedDigest);

  // --- Verify every row's split agrees with the family assignment ---
  const famSplit = new Map<string, SplitName>();
  for (const fid of family_split.development) famSplit.set(fid, "development");
  for (const fid of family_split.calibration) famSplit.set(fid, "calibration");
  for (const fid of family_split.holdout) famSplit.set(fid, "holdout");
  for (const row of rows) {
    const assigned = famSplit.get(row.family_id);
    if (assigned !== row.split) {
      throw new Error(`family split mismatch: ${row.id} (${row.split} != ${assigned})`);
    }
  }

  // --- Verify family disjointness: same family never spans splits ---
  for (const family of families) {
    const splits = new Set(
      rows.filter((r) => r.family_id === family.family_id).map((r) => r.split),
    );
    if (splits.size > 1) {
      throw new Error(`family ${family.family_id} spans multiple splits`);
    }
  }

  // --- Digests ---
  const prompt_digest = sha256(
    lexicalJson({ domain: domain.domain, version: domain.version, prompt: domain.prompt }),
  );
  const rows_digest = sha256(
    lexicalJson(
      rows
        .map((r) => ({
          id: r.id,
          content_digest: r.content_digest,
          family_id: r.family_id,
          split: r.split,
          expected: r.expected,
          review_digest: r.review_digest,
        }))
        .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)),
    ),
  );
  const family_split_digest = sha256(lexicalJson(family_split));

  const manifest: DatasetManifest = {
    version: 2,
    domain: domain.domain,
    domain_version: domain.version,
    prompt_digest,
    rows_digest,
    family_split_digest,
    review_freeze_digest: freeze ? freeze.freeze_digest : null,
  };

  // Logical digest excludes the freeze pointer.
  const dataset_digest = sha256(
    lexicalJson({
      domain: domain.domain,
      version: domain.version,
      prompt: domain.prompt,
      rows: rows
        .slice()
        .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
        .map((r) => ({
          content_digest: r.content_digest,
          family_id: r.family_id,
          split: r.split,
          expected: r.expected,
          review_digest: r.review_digest,
        })),
      family_split,
      review_ledger: rows.map((r) => r.review_digest).sort(),
    }),
  );

  return {
    manifest,
    rows,
    family_split,
    dataset_digest,
  };
}