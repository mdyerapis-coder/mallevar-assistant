// Canonical wire DTOs have a single Rust-owned generated definition.
export type { Digest, JudgmentInput, JudgmentRecord, ScoreEvidence } from "../../../contracts/src/generated";
import type { JudgmentInput } from "../../../contracts/src/generated";

export type JevTransport = (url: string, init: RequestInit) => Promise<Response>;
export interface JevClientOptions {
  // Resolve only already-approved local content. The exact UTF-8 digest must match
  // input_digest. This adapter does not load references or grant disclosure.
  resolveInput(input: JudgmentInput, signal: AbortSignal): Promise<string>;
  transport?: JevTransport;
  /** May shorten, never extend, the two-second end-to-end bound. */
  timeout_ms?: number;
  /** Per-client bound; File Steward's shared runtime is not exclusively leased. */
  max_in_flight?: number;
}
export interface RuntimeIdentity {
  runtime_base: string;
  digest: string;
  model_identity: string;
  model_path: string;
  build_info: string;
  // Lexical reported metadata only; no claim about model bytes, process PID,
  // start time, backend memory, or absence of changes between observations.
  assurance: "reported_metadata_only";
}
export type DecodedChoice =
  | { state: "available"; choice: string }
  | { state: "abstain" }
  | { state: "invalid" };
export type CandidateCodes = Readonly<Record<string, string>>;
