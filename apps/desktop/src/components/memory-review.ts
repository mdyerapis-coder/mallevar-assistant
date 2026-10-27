// Approved-memory review: lists memory records (candidate/approved/
// forgotten — `MemoryRecord.state` is the core's free-form state) and drives
// the P5 decision commands (`memory_propose`, `memory_decide`,
// `memory_forget`). Approving a candidate is a real core decision — the UI
// only sends the decision; the core commits it. Editing a candidate sends
// `edit_and_approve` with the corrected text.
import type { MemoryDecision, MemoryRecord } from "@ironbark/contracts";
import { CoreError } from "../core-client";
import { datetimeText, h, note, pill } from "./dom";

export interface MemoryHandlers {
  onPropose: (text: string, provenance: string) => Promise<void>;
  onDecide: (memoryId: string, decision: MemoryDecision, editedText?: string) => Promise<void>;
  onForget: (memoryId: string) => Promise<void>;
}

function coreFailureNote(err: unknown): HTMLElement {
  const code = err instanceof CoreError ? err.code : "transport";
  const message = err instanceof Error ? err.message : String(err);
  return note(`core ${code}: ${message}`, "err");
}

/** One memory record row with its real decision controls. Candidate records
 * expose approve / edit-and-approve / reject; approved records expose forget.
 * A record whose state is neither renders its state verbatim with no
 * fabricated controls. */
export function memoryRow(record: MemoryRecord, handlers: MemoryHandlers): HTMLElement {
  const card = h("div", { class: "memory-card" },
    h("div", { class: "memory-text" }, record.text),
    h("div", { class: "memory-meta" },
      `id: ${record.id} · state: ${record.state} · provenance: ${record.provenance} · ${datetimeText(record.created_at)}`,
    ),
  );

  const status = h("span", { class: "memory-status" });
  const actions = h("div", { style: "display:flex;gap:8px;margin-top:8px;align-items:center;flex-wrap:wrap;" },
    pill(statePillClass(record.state), record.state),
  );

  if (isCandidate(record.state)) {
    const approve = h("button", { class: "primary", type: "button" }, "Approve");
    approve.addEventListener("click", async () => {
      status.replaceChildren(note("Sending approval…"));
      try {
        await handlers.onDecide(record.id, "approve");
        status.replaceChildren(note("Approved. The core recorded the decision.", ""));
      } catch (err) {
        status.replaceChildren(coreFailureNote(err));
      }
    });

    const editButton = h("button", { type: "button" }, "Edit and approve");
    editButton.addEventListener("click", async () => {
      const text = window.prompt("Edit the memory text before approving:", record.text);
      if (text === undefined) return;
      status.replaceChildren(note("Sending edit and approve…"));
      try {
        await handlers.onDecide(record.id, "edit_and_approve", text);
        status.replaceChildren(note("Edited and approved. The core recorded the decision.", ""));
      } catch (err) {
        status.replaceChildren(coreFailureNote(err));
      }
    });

    const reject = h("button", { class: "danger", type: "button" }, "Reject");
    reject.addEventListener("click", async () => {
      status.replaceChildren(note("Sending rejection…"));
      try {
        await handlers.onDecide(record.id, "reject");
        status.replaceChildren(note("Rejected. The core recorded the decision.", ""));
      } catch (err) {
        status.replaceChildren(coreFailureNote(err));
      }
    });

    actions.append(approve, editButton, reject, status);
  } else if (isApproved(record.state)) {
    const forget = h("button", { class: "danger", type: "button" }, "Forget");
    forget.addEventListener("click", async () => {
      if (!window.confirm(`Forget memory ${record.id}? The record is marked forgotten; the text is no longer retrieved.`)) return;
      status.replaceChildren(note("Forgetting…", "info"));
      try {
        await handlers.onForget(record.id);
        status.replaceChildren(note("Forgotten. The core recorded the decision.", ""));
      } catch (err) {
        status.replaceChildren(coreFailureNote(err));
      }
    });
    actions.append(forget, status);
  } else {
    actions.append(h("span", { class: "muted", style: "font-size:12px;" }, "no decision controls for this state"), status);
  }

  card.append(actions);
  return card;
}

function isCandidate(state: string): boolean {
  return state === "candidate" || state === "pending" || state === "proposed";
}

function isApproved(state: string): boolean {
  return state === "approved" || state === "active";
}

function statePillClass(state: string): string {
  if (isCandidate(state)) return "awaiting_approval";
  if (isApproved(state)) return "succeeded";
  if (state === "forgotten" || state === "rejected") return "cancelled";
  return "unknown";
}
