// Automation tool adapter for the Ironbark automation broker.
//
// The trusted core is the single front door for browser (`BrowserProfile`),
// desktop (`NativeWindow`) and core-owned `document_file` automation:
// `AutomationBroker::observe/execute/release`
// (crates/ironbark-core/src/automation/broker.rs). A coding worker NEVER has
// host-session or personal-browser authority — it may only issue scoped broker
// requests, and this adapter is the typed TS client for those requests. It
// dispatches the generated wire commands (`CoreCommand`) and returns the
// generated DTOs from `@ironbark/contracts`.
//
//   ironbark automation observe <target>
//   ironbark automation execute <action> <grant>
//   ironbark automation release <scope>
//
// Authority is enforced by the core, never here:
//  * `observe` opens the durable scope and returns the target's live revision
//    digest. `execute` refuses a released scope, re-observes the target and
//    refuses a stale snapshot, and validates the exact grant — so an execute
//    call with no fresh target observation fails (`scope_missing`/`stale_target`).
//    The caller must bind `action.expected_state_digest` to a digest from a live
//    `observe`; this adapter forwards that binding as the command's
//    `expected_state_digest` and never invents one.
//  * `execute` passes the real `ActionSpec` + `ApprovalGrant` through unchanged;
//    it never synthesises a grant.
//  * Disclosures (screenshot / cookies / clipboard / upload) require an
//    owner-issued `ConsentRecord`. Only the owner's adapter records one; a
//    page-world or coding-worker request can never approve a disclosure, so this
//    adapter never fabricates consent and lets the core answer
//    `consent_required`.
//  * The broker's typed error is surfaced to the caller, never swallowed. This
//    adapter never touches the host session bus or the operator's personal
//    browser directly; every action is a broker request.

import type {
  ActionSpec,
  ApprovalGrant,
  ApprovedTarget,
  AutomationObservation,
  AutomationReceipt,
  ConsentKind,
  ConsentRecord,
  ConsentSource,
  CoreCommand,
  CoreReply,
  Effect,
  ResultOf,
  ScopeRole,
  TargetKind,
} from '../../../contracts/src/generated.ts';
import { validateResult } from '../../../contracts/src/validate.ts';

/** Re-exports from the automation module for convenience. */
export type {
  ActionSpec,
  ApprovalGrant,
  ApprovedTarget,
  AutomationObservation,
  AutomationReceipt,
  ConsentKind,
  ConsentRecord,
  ConsentSource,
  Effect,
  ScopeRole,
  TargetKind,
};

/**
 * The broker's typed error, reconstructed from the core's `CoreReply::Error`.
 * The `code` is the stable capability/policy signal (`scope_missing`,
 * `stale_target`, `consent_required`, `consent_owner_required`, `grant_denied`,
 * `unsupported`, ...). It is rethrown verbatim so the caller always sees the
 * real broker outcome instead of a generic failure.
 */
export class CoreError extends Error {
  readonly code: string;
  readonly details: unknown;
  constructor(code: string, message: string, details?: unknown) {
    super(message);
    this.name = 'CoreError';
    this.code = code;
    this.details = details;
  }
}

/**
 * The transport half of a durable core client. The same-user core transports
 * (the `ironbark-cli` generic `command` surface and the desktop Tauri client)
 * satisfy this: one `CoreCommand` in, the core's raw `CoreReply` out. Keeping the
 * unwrapped `CoreReply` here lets the adapter surface the broker's typed error.
 */
export interface AutomationCoreTransport {
  command(command: CoreCommand, requestId?: string): Promise<CoreReply>;
}

/**
 * Typed client for the Ironbark automation broker.
 */
export class AutomationAdapter {
  constructor(private readonly core: AutomationCoreTransport) {}

  /** Dispatch one broker command and unwrap the reply's matching leaf.
   *  `validateResult` proves (against the generated schema) that the reply
   *  envelope is exactly this command's variant — a mismatched or echoed
   *  payload is rejected — and the broker's typed error is rethrown verbatim
   *  rather than swallowed. */
  private async dispatch<C extends CoreCommand>(command: C, requestId?: string): Promise<ResultOf<C>> {
    const reply = await this.core.command(command, requestId);
    if (reply.type === 'error') throw new CoreError(reply.code, reply.message, reply.details);
    return validateResult(command, reply.value);
  }

  /**
   * Open a scoped observation of a target (no side effects) and return its live
   * state. The returned `revision_digest` is the freshness the next execute must
   * be bound to via `action.expected_state_digest`.
   */
  async observe(target: ApprovedTarget): Promise<AutomationObservation> {
    return this.dispatch({ type: 'automation_observe', target });
  }

  /**
   * Execute one authorised action. The real `ActionSpec` and `ApprovalGrant` are
   * passed through unchanged; this adapter never mints a grant, and a disclosure
   * still requires an owner-issued `ConsentRecord` that only the owner can record.
   */
  async execute(action: ActionSpec, grant: ApprovalGrant, requestId?: string): Promise<AutomationReceipt> {
    return this.dispatch(
      {
        type: 'automation_execute',
        action,
        grant,
        expected_state_digest: action.expected_state_digest,
      },
      requestId,
    );
  }

  /**
   * Release an owner-issued scope (idempotent), making it ineligible for further
   * use.
   */
  async release(scopeId: string): Promise<void> {
    await this.dispatch({ type: 'automation_release', scope_id: scopeId });
  }
}
