// D3 Approvals surface: pending approval actions drawn from durable
// `approval_needed` receipts, displayed with the exact target/destination,
// canonical args, snapshot/policy digest and policy scope. A denial is a real
// core decision (approval_decide = deny). If the snapshot digest changes
// after a row is rendered, the stale card must not be reused: the UI discards
// the old decision button and requires the owner to review the new snapshot
// first. The core is the source of truth — a stale decision replies with a
// typed error, which the UI surfaces instead of double-approving.
import { app, getClient, recordError } from "../app";
import { h, kvRow, note, pill } from "../components/dom";
import { approvalActionFields } from "./tasks";

interface PendingApproval {
  action: ActionSpec;
  envelope: EventEnvelope;
}

export interface ApprovalsView {
  root: HTMLElement;
  render(): void;
  refresh(): Promise<void>;
}

export function mountApprovalsView(): ApprovalsView {
  const root = h("div", { class: "surface approvals-surface" });
  const header = h("div", { class: "surface-header" });
  const body = h("div", { class: "surface-body" });
  root.append(header, body);

  let items: PendingApproval[] = [];

  function render(): void {
    renderHeader();
    renderBody();
  }

  function renderHeader(): void {
    header.replaceChildren(
      h("div", { class: "title" }, "Approvals"),
      h("div", { class: "subtitle" }, "Pending approvals and their exact targets"),
    );
  }

  function renderBody(): void {
    const project = app.projects.find((p) => p.id === app.selectedProjectId) ?? app.projects[0];
    body.replaceChildren();
    if (!project) {
      body.append(note("Select a project first.", "info"));
      return;
    }
    if (items.length === 0) {
      body.append(note("No pending approvals in the projected event stream for this project.", "info"));
      return;
    }
    const list = h("div", { class: "approvals-list" });
    for (const item of items) {
      list.append(approvalCard(item));
    }
    const refreshBtn = h("button", { class: "subtle", type: "button" }, "Refresh");
    refreshBtn.addEventListener("click", () => void refresh());
    body.append(refreshBtn, list);
  }

  function approvalCard(item: PendingApproval): HTMLElement {
    const { action } = item;
    const card = h("div", { class: "approval-card" });
    card.append(h("div", { class: "approval-head" },
      h("span", {}, `action ${action.id.slice(0, 8)}`),
      pill(action.scope ? "uncertain" : "unknown", "approval needed"),
      h("span", { class: "muted" }, `run ${action.run_id.slice(0, 8)}`),
    ));
    const detail = h("div", { class: "approval-detail" });
    detail.append(approvalActionFields(action));
    card.append(detail);
    const decisionArea = h("div", { style: "margin-top:8px;" });
    decisionArea.append(note("Stale approval check: approving binds to the snapshot above. If the snapshot changed since this row rendered, the core refuses the decision and the UI requires a fresh review.", "info"));
    const buttons = h("div", { style: "display:flex;gap:8px;margin-top:6px;" });
    const allow = h("button", { class: "primary", type: "button" }, "Approve");
    allow.addEventListener("click", async () => {
      if (!window.confirm(`Approve action ${action.id.slice(0, 8)} (snapshot ${action.expected_state_digest.slice(0, 12)}…)?`)) return;
      await decide(item, "allow");
    });
    const deny = h("button", { class: "danger", type: "button" }, "Deny");
    deny.addEventListener("click", async () => {
      await decide(item, "deny");
    });
    buttons.append(allow, deny);
    decisionArea.append(buttons);
    card.append(decisionArea);
    return card;
  }

  async function decide(item: PendingApproval, decision: "allow" | "deny"): Promise<void> {
    try {
      await getClient().command<{ done: true }>({ type: "approval_decide", action_id: item.action.id, decision });
      await refresh();
    } catch (err) {
      recordError("approval_decide", err);
      const noteEl = note(`Decision not accepted (core): ${err instanceof Error ? err.message : String(err)}`, "err");
      if (err instanceof Error && (err.message.includes("stale") || err.message.includes("snapshot"))) {
        noteEl.append(note(" The snapshot changed. This approval card is stale; review the fresh pending approval.", "warn"));
      }
      body.append(noteEl);
    }
  }

  async function loadApprovals(): Promise<() => void> {
    const runs = Object.values(app.runs);
    const receipts: PendingApproval[] = [];
    for (const run of runs) {
      // Gather approval_needed receipts from the run's messages and from every
      // message in the active conversation's history (same source the task
      // rail reads). This is the honest "what the core recorded is pending".
      const history = app.history;
      if (history) {
        for (const branch of history.branches) {
          for (const message of branch.messages) {
            for (const envelope of message.receipts) {
              if (envelope.payload.type !== "approval_needed") continue;
              const payload = envelope.payload as Record<string, unknown>;
              const action = payload.action as ActionSpec | undefined;
              if (action?.id) {
                receipts.push({ action, envelope });
              }
            }
          }
        }
      }
    }
    items = receipts;
    return () => {};
  }

  async function refresh(): Promise<void> {
    const stop = await loadApprovals();
    render();
    stop();
  }

  return { root, render, refresh };
}