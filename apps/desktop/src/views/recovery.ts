// D3 Recovery surface: the observed checkpoint/workspace state for a run and
// the four real reconciliation choices (AcceptObserved / ResumeReadOnly /
// ContinueFromCheckpoint / Abandon). There is never a blind "Retry": when an
// uncertain mutation is pending the UI only offers the reconciliation that
// matches the observed state.
import { app, coreErrorNote, getClient, recordError } from "../app";
import { digestView, h, jsonSummary, kvRow, note, pill } from "../components/dom";

export interface RecoveryView {
  root: HTMLElement;
  render(): void;
  refresh(): Promise<void>;
}

const DECISIONS: Array<{ value: RecoveryDecision; label: string; description: string }> = [
  { value: "accept_observed", label: "Accept observed state", description: "Treat the observed checkpoint/workspace as the committed state for any continued work." },
  { value: "resume_read_only", label: "Resume read-only", description: "Observe further state without resuming mutation." },
  { value: "continue_from_checkpoint", label: "Continue from checkpoint", description: "Resume mutation from the durable checkpoint." },
  { value: "abandon", label: "Abandon", description: "Record the run as abandoned; no further mutation is resumed." },
];

function isRecoveryDecision(value: string): value is RecoveryDecision {
  return DECISIONS.some((d) => d.value === value);
}

export function mountRecoveryView(): RecoveryView {
  const root = h("div", { class: "surface recovery-surface" });
  const header = h("div", { class: "surface-header" });
  const body = h("div", { class: "surface-body" });
  root.append(header, body);

  let reports: Map<string, RecoveryReport> = new Map();
  let loading: Record<string, boolean> = {};
  let selected: string | undefined;

  function render(): void {
    renderHeader();
    renderSelector();
    renderDetail();
  }

  function renderHeader(): void {
    header.replaceChildren(
      h("div", { class: "title" }, "Recovery"),
      h("div", { class: "subtitle" }, "Observed checkpoint/workspace state and reconciliation choices"),
    );
  }

  function candidateRuns(): string[] {
    // Runs the app projects in a non-terminal state, plus the most recent run
    // overall so the owner can pick one to investigate.
    const runs = Object.values(app.runs);
    const active = runs.filter((r) => !["succeeded", "failed", "cancelled", "abandoned"].includes(r.status));
    const candidates = active.length > 0
      ? active
      : runs.slice().sort((a, b) => b.run_id.localeCompare(a.run_id)).slice(0, 1);
    return candidates.map((r) => r.run_id);
  }

  function renderSelector(): void {
    const selector = h("div", { class: "panel" });
    selector.append(h("h2", {}, "Run"));
    const select = h("select", { "aria-label": "Run" });
    const candidates = candidateRuns();
    if (selected && !candidates.includes(selected)) selected = undefined;
    if (!selected && candidates.length > 0) selected = candidates[0];
    for (const runId of candidates) {
      select.append(h("option", { value: runId, selected: runId === selected }, `${runId.slice(0, 8)}`));
    }
    if (candidates.length === 0) {
      select.append(h("option", { value: "" }, "No runs observed"));
    }
    select.addEventListener("change", () => {
      selected = select.value || undefined;
      renderDetail();
    });
    const refreshBtn = h("button", { class: "subtle", type: "button" }, "Probe recovery");
    refreshBtn.addEventListener("click", () => {
      if (selected) void probeRun(selected);
    });
    selector.append(select, refreshBtn);
    body.replaceChildren(selector);
  }

  function reportFor(runId: string): RecoveryReport | undefined {
    return reports.get(runId);
  }

  function renderDetail(): void {
    const detail = h("div", { class: "panel" });
    detail.append(h("h2", {}, "Observed state"));
    if (!selected) {
      detail.append(note("No run selected.", "info"));
      body.append(detail);
      return;
    }
    const report = reportFor(selected);
    if (!report) {
      detail.append(note(`No recovery report for run ${selected.slice(0, 8)}. Press "Probe recovery" to query the core.`, "info"));
      body.append(detail);
      return;
    }
    detail.append(kvRow("run", report.run_id), kvRow("run state", report.run_state));
    if (report.recorded_pid !== undefined) detail.append(kvRow("recorded pid", String(report.recorded_pid)));
    if (report.recorded_start_time !== undefined) detail.append(kvRow("recorded start", String(report.recorded_start_time)));
    if (report.checkpoint) {
      const cp = report.checkpoint;
      detail.append(kvRow("checkpoint phase", cp.phase), kvRow("state digest", digestView(cp.state_digest)), kvRow("checkpoint at", new Date(cp.created_at).toISOString()));
      detail.append(kvRow("manifest", jsonSummary(cp.manifest, 300)));
    } else {
      detail.append(kvRow("checkpoint", "none recorded"));
    }
    detail.append(kvRow("observed workspace", jsonSummary(report.observed_workspace, 300)));
    detail.append(kvRow("pending mutation", report.pending_mutation ? "yes — uncertain mutation in flight" : "no"));
    if (report.uncertain_actions.length > 0) {
      const list = h("div");
      for (const action of report.uncertain_actions) list.append(h("div", {}, `· ${action}`));
      detail.append(kvRow("uncertain actions", ""), list);
    }
    if (report.recommended.length > 0) {
      const recs = h("div", { style: "display:flex;gap:6px;flex-wrap:wrap;" });
      for (const rec of report.recommended) recs.append(pill("focused", rec));
      detail.append(kvRow("recommended", ""), recs);
    }

    // Reconciliation actions (never a blind Retry).
    const actions = h("div", { class: "recovery-actions", style: "margin-top:12px;display:flex;gap:8px;flex-wrap:wrap;" });
    const canContinue = report.checkpoint && !report.pending_mutation;
    for (const d of DECISIONS) {
      const disabled = d.value === "continue_from_checkpoint" && !canContinue;
      const btn = h("button", { class: d.value === "abandon" ? "danger" : "primary", type: "button", disabled }, d.label);
      btn.title = disabled ? "No durable checkpoint available — continue is not safe" : d.description;
      btn.addEventListener("click", () => void choose(d.value));
      actions.append(btn);
    }
    if (report.pending_mutation) {
      actions.append(note("An uncertain mutation is pending — only Accept observed / Resume read-only / Abandon are safe here.", "warn"));
    }
    detail.append(actions);
    body.append(detail);
  }

  async function probeRun(runId: string): Promise<void> {
    loading[runId] = true;
    try {
      const report = await getClient().command<RecoveryReport>({ type: "run_recovery", run_id: runId });
      reports.set(runId, report);
    } catch (err) {
      recordError("run_recovery", err);
      body.append(coreErrorNote(err));
    }
    loading[runId] = false;
    render();
  }

  async function choose(decision: RecoveryDecision): Promise<void> {
    if (!selected) return;
    if (decision === "continue_from_checkpoint" && reportFor(selected)?.pending_mutation) {
      if (!window.confirm("This run has a pending mutation. Continue from checkpoint may replay partial state. Proceed?")) return;
    }
    if (decision === "abandon") {
      if (!window.confirm(`Abandon run ${selected.slice(0, 8)}? It will be marked abandoned; no further mutation is resumed.`)) return;
    }
    try {
      await getClient().command<{ done: true }>({ type: "run_reconcile", run_id: selected, decision });
      reports.delete(selected);
      render();
    } catch (err) {
      recordError("run_reconcile", err);
      body.append(coreErrorNote(err));
    }
  }

  async function refresh(): Promise<void> {
    if (selected) await probeRun(selected);
    render();
  }

  return { root, render, refresh };
}