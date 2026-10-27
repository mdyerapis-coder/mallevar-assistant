// qa/qualification/jev/registry.ts — shared loading/identity helpers for the J2
// qualification scripts.
//
// Fixture rows are validated exactly as the domain builder validates them
// (content_digest must equal sha256(text)); a raw JSON line is never cast into a
// FixtureRow. Judgment decision ids are projected into UUID form because the core
// contract types DecisionId as a UUID, and the projection is deterministic so a
// replayed row keeps the same identity. The observed runtime identity is read in
// exactly one place, so a calibration receipt and an evaluation report bind to
// the same digest.
import { loadJsonl, validateFixtureRows } from "../../../src/jev/domains/types";
import type { DomainDefinition, FixtureRow } from "../../../src/jev/domains/types";
import { sha256 } from "../../../src/jev/identity";
import { JevClient } from "../../../src/jev/client";
import type { JevTransport, RuntimeIdentity } from "../../../src/jev/types";

export async function loadFixtureRows(domain: DomainDefinition): Promise<FixtureRow[]> {
  const root = process.env.IRONBARK_ROOT ?? process.cwd();
  const rows: FixtureRow[] = [];
  for (const f of domain.fixtures) {
    const text = await Bun.file(resolveFixture(root, f)).text();
    rows.push(...validateFixtureRows(loadJsonl(text)));
  }
  return rows;
}

/**
 * Offline transport for structural runs. It answers every facade/runtime route
 * with a non-identity payload, so the client reports `unqualified` instead of
 * inventing a choice; no live model is contacted.
 */
export const offlineTransport: JevTransport = () =>
  Promise.resolve(new Response('{"state":"abstain","choice":"ABSTAIN"}', {
    headers: { "content-type": "application/json" },
  }));

/** Evaluation, calibration and actual judgments use the same strict probe. */
export async function probeRuntimeIdentity(transport: JevTransport): Promise<RuntimeIdentity | null> {
  try {
    return await new JevClient({
      transport,
      resolveInput: async () => { throw new Error("identity probe does not resolve task content"); },
    }).observeIdentity(AbortSignal.timeout(2000));
  } catch {
    return null;
  }
}

/**
 * Deterministic decision id for a judgment label. A judgment's decision_id is a
 * UUID in the core contract, so a fixture id cannot be used directly; the same
 * label always yields the same id, keeping replays idempotent.
 */
export function decisionIdFor(label: string): string {
  const nibbles = sha256(`ironbark-jev:decision-id:v1:${label}`).slice(0, 32).split("");
  nibbles[12] = "4"; // version 4
  nibbles[16] = "8"; // RFC 4122 variant
  const hex = nibbles.join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`;
}

function resolveFixture(root: string, f: string): string {
  if (f.startsWith("http://") || f.startsWith("https://") || f.startsWith("file://")) return f;
  return root.endsWith("/") ? root + f : root + "/" + f;
}
