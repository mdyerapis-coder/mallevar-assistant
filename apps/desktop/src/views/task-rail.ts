// D2 collapsible right task rail. Projects the live run/task state for the
// active conversation: status, pinned route, native session identity, receipt
// trail and usage truth. `TaskRail.render(run: RunProjection | null)` per the
// D2 interface.
//
// Truthfulness: native children that the harness reports opaquely (or whose
// usage is unknown) stay labelled "opaque native identity" / "usage unknown" —
// never guessed. An empty rail says so plainly.
import { app } from "../app";
import { datetimeText, h, jsonSummary, note, pill } from "../components/dom";
import type { ReceiptLine } from "../components/receipts";
import type { SessionRef, UsageObservation } from "@ironbark/contracts";

export interface RunProjection {
  runId: string;
  status: string;
  route?: { connection_id: string; model_id: string };
  session?: { harness: string; native_id: string };
  receipts: ReceiptLine[];
}

export interface TaskRailHandle {
  root: HTMLElement;
  render(run: RunProjection | null): void;
}

export function mountTaskRail(): TaskRailHandle {
  const root = h("aside", { class: "task-rail", "aria-label": "Task rail" });
  const head = h("div", { class: "task-rail-head" },
    h("div", {}, h("div", { class: "title" }, "Task"), h("div", { class: "subtitle muted", style: "font-size:12px;" }, "scope of the active run")),
    h("button", { class: "icon subtle", type: "button", "aria-label": "Collapse task rail" }, "⇥"),
  );
  const body = h("div", { class: "task-rail-body" });
  root.append(head, body);

  const toggle = head.querySelector("button") as HTMLButtonElement;
  toggle.addEventListener("click", () => {
    app.railCollapsed = !app.railCollapsed;
    applyCollapsed();
    toggle.setAttribute("aria-label", app.railCollapsed ? "Expand task rail" : "Collapse task rail");
  });

  function applyCollapsed(): void {
    root.classList.toggle("collapsed", app.railCollapsed);
  }
  applyCollapsed();

  function render(run: RunProjection | null): void {
    body.replaceChildren();
    if (!run) {
      body.append(note("No active run in this conversation. Queued turns, tool receipts and approval requests appear here as they occur.", "info"));
      return;
    }

    body.append(h("div", { style: "display:flex;justify-content:space-between;align-items:center;gap:8px;" },
      h("span", {}, "run"),
      h("code", { class: "inline" }, run.runId.slice(0, 8)),
      pill(run.status),
    ));

    if (run.route) {
      body.append(h("div", { class: "note" }, `route: ${run.route.connection_id}/${run.route.model_id}`));
    } else {
      body.append(note("No route recorded for this run.", "info"));
    }

    if (run.session) {
      body.append(h("div", { class: "note" },
        `native session: ${run.session.harness} · ${run.session.native_id} (opaque native identity; not expandable in the webview)`,
      ));
    } else {
      body.append(note("No native session observed yet. If the harness reports children opaquely, they stay labelled unknown here.", "info"));
    }

    const receipts = h("div", { class: "receipts" });
    if (run.receipts.length === 0) {
      receipts.append(note("No receipts recorded for this run yet.", "info"));
    }
    for (const line of run.receipts) receipts.append(receiptLineNode(line));
    body.append(h("h3", { style: "margin:10px 0 6px;font-size:14px;" }, "Receipts"), receipts);

    const usage = usageNote(run.receipts);
    if (usage) body.append(usage);
  }

  function receiptLineNode(line: ReceiptLine): HTMLElement {
    const el = h("div", { class: `receipt ${line.state}` },
      h("div", { class: "receipt-head" },
        h("span", { class: "receipt-kind" }, line.kind),
        pill(line.state),
        h("span", { class: "muted" }, datetimeText(line.at)),
      ),
    );
    if (line.detail) el.append(h("div", { class: "receipt-detail" }, line.detail));
    return el;
  }

  function usageNote(receipts: ReceiptLine[]): HTMLElement | null {
    const unknown = receipts.some((r) => r.state === "uncertain" || r.state === "unknown");
    if (!unknown) return null;
    return note("Some receipts have unknown or uncertain usage: the core did not report an observed outcome, so no effect is claimed here.", "warn");
  }

  return { root, render };
}

// ── Projections from app state ───────────────────────────────────────────

/** Builds the rail projection for one run from app state: status, pinned
 * route (message snapshot) and receipt lines (durable envelopes). Unknown
 * native child/usage states stay labelled. */
export function runProjectionFor(runId: string): RunProjection | null {
  const run = app.runs[runId];
  if (!run) return null;
  return {
    runId: run.run_id,
    status: run.status,
    route: findRoute(run.message_id),
    session: findSession(run.message_id),
    receipts: receiptsForRun(runId),
  };
}

function findRoute(messageId: string | undefined): { connection_id: string; model_id: string } | undefined {
  const history = app.history;
  if (!history || !messageId) return undefined;
  for (const branch of history.branches) {
    const message = branch.messages.find((m) => m.id === messageId);
    if (message?.snapshot) return message.snapshot.route;
  }
  return undefined;
}

function findSession(messageId: string | undefined): { harness: string; native_id: string } | undefined {
  const history = app.history;
  if (!history || !messageId) return undefined;
  for (const branch of history.branches) {
    for (const message of branch.messages) {
      if (message.id !== messageId) continue;
      for (const envelope of message.receipts) {
        if (envelope.payload.type !== "session_bound") continue;
        const payload = envelope.payload as Record<string, unknown>;
        const session = payload.session;
        if (session && typeof session === "object") {
          const record = session as Record<string, unknown>;
          if (typeof record.harness === "string" && typeof record.native_id === "string") {
            return { harness: record.harness, native_id: record.native_id };
          }
        }
      }
    }
  }
  return undefined;
}

/** The full SessionRef for a run (harness, native identity, cwd, resolved
 * route) when the harness bound one durably. */
export function sessionForRun(runId: string): SessionRef | undefined {
  const run = app.runs[runId];
  if (!run?.message_id) return undefined;
  const history = app.history;
  if (!history) return undefined;
  for (const branch of history.branches) {
    for (const message of branch.messages) {
      if (message.id !== run.message_id) continue;
      for (const envelope of message.receipts) {
        if (envelope.payload.type !== "session_bound") continue;
        const payload = envelope.payload as Record<string, unknown>;
        return payload.session as SessionRef | undefined;
      }
    }
  }
  return undefined;
}

function receiptsForRun(runId: string): ReceiptLine[] {
  const history = app.history;
  if (!history) return [];
  const lines: ReceiptLine[] = [];
  for (const branch of history.branches) {
    for (const message of branch.messages) {
      if (message.run_id !== runId) continue;
      for (const envelope of message.receipts) {
        const line = receiptLineFromEnvelope(envelope);
        if (line) lines.push(line);
      }
    }
  }
  return lines.sort((a, b) => a.at - b.at);
}

interface ActionFields {
  id?: string;
  kind?: string;
  destination?: string;
}

function actionFields(value: unknown): ActionFields | undefined {
  if (!value || typeof value !== "object") return undefined;
  const record = value as Record<string, unknown>;
  return {
    id: typeof record.id === "string" ? record.id : undefined,
    kind: typeof record.kind === "string" ? record.kind : undefined,
    destination: typeof record.destination === "string" ? record.destination : undefined,
  };
}

function usageFields(value: unknown): UsageObservation | undefined {
  if (!value || typeof value !== "object") return undefined;
  const record = value as Record<string, unknown>;
  if (typeof record.source !== "string") return undefined;
  return record as unknown as UsageObservation;
}

function str(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function receiptLineFromEnvelope(envelope: { timestamp: number; payload: { type: string; [key: string]: unknown } }): ReceiptLine | null {
  const p = envelope.payload;
  switch (p.type) {
    case "tool_started": {
      const action = actionFields(p.action);
      return {
        kind: action?.kind ?? "tool",
        state: "executing",
        at: envelope.timestamp,
        detail: action?.destination ? `destination: ${action.destination}` : "no destination reported",
      };
    }
    case "tool_finished":
      return {
        kind: "tool finished",
        state: "succeeded",
        at: envelope.timestamp,
        detail: `receipt: ${jsonSummary(p.result, 160)}`,
      };
    case "approval_needed": {
      const action = actionFields(p.action);
      return {
        kind: action?.kind ?? "approval needed",
        state: "awaiting_approval",
        at: envelope.timestamp,
        detail: `action ${action?.id ?? "unknown"}`,
      };
    }
    case "error":
      return { kind: "error", state: "failed", at: envelope.timestamp, detail: `${str(p.code) ?? "error"}: ${str(p.message) ?? ""}` };
    case "turn_finished":
      return { kind: "turn finished", state: str(p.status) ?? "unknown", at: envelope.timestamp };
    case "usage_observed": {
      const usage = usageFields(p.usage);
      return {
        kind: "usage",
        state: usage?.source === "provider" ? "succeeded" : "unknown",
        at: envelope.timestamp,
        detail: usage && (usage.input_tokens !== undefined || usage.output_tokens !== undefined)
          ? `in ${usage.input_tokens ?? "?"} / out ${usage.output_tokens ?? "?"} (source: ${usage.source ?? "unknown"})`
          : "usage unknown — not reported by the provider",
      };
    }
    default:
      return null;
  }
}
