import type { JudgmentState } from '../../../contracts/src/generated';

const IDENTITY_FAILURES: Readonly<Record<string, true>> = {
  runtime_identity_changed: true,
  runtime_identity_unknown: true,
  completion_model_mismatch: true,
};

/** Continuity of observed metadata only, never proof of loaded model bytes. */
export class EvaluationRunIdentity {
  #identity: string | null = null;
  #observed = false;
  #continuous = true;
  #reasons: string[] = [];

  observe(recordIdentity: string | null, state: JudgmentState, identityFailure?: string): void {
    this.#observed = true;
    const reason = identityFailure && Object.hasOwn(IDENTITY_FAILURES, identityFailure) ? identityFailure
      : recordIdentity === null ? `runtime_identity_missing:${state}`
      : this.#identity !== null && recordIdentity !== this.#identity ? 'runtime_identity_changed' : null;
    if (reason !== null) {
      this.#continuous = false;
      if (!this.#reasons.includes(reason)) this.#reasons.push(reason);
    }
    if (recordIdentity !== null && this.#identity === null) this.#identity = recordIdentity;
  }

  finish(): { identity_digest: string | null; continuous: boolean; reasons: string[] } {
    const continuous = this.#observed && this.#continuous && this.#identity !== null;
    return {
      identity_digest: continuous ? this.#identity : null,
      continuous,
      reasons: this.#observed ? [...this.#reasons] : ['runtime_identity_unobserved'],
    };
  }
}
