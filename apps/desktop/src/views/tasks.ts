// D3 Tasks surface: the task graph for runs the app projects — roles, native
// harness identities, dependencies, tool receipts, terminal/log output, and
// the review/verification + integration outcome.
//
// Every value comes from the run/message projections and durable receipts.
// Opaque native children and unknown usage are labelled, never guessed; where
// the core has not yet recorded a value (e.g. a result manifest, or a diff
// body), the view shows the real "not recorded" state instead of inventing it.
import type { MessageView } from "@ironbark/contracts";
import { app } from "../app";
import { digestView, h, jsonSummary, kvRow, note, pill } from "../components/dom";
import { renderGraph, type GraphNodeInput } from "../components/graph";
import { renderReceipts, type ReceiptLine } from "../components/receipts";
import { mountTerminalView, type TerminalLine, type TerminalViewHandle } from "../components/terminal";
import { runProjectionFor, sessionForRun } from "./task-rail";
import type { EventEnvelope } from "../core-client";

/** Collects the run rows the app currently projects. */
function runRows(): RunRow[] {
  const rows: RunRow[] = [];
  for (const run of Object.values(app.runs)) {
    rows.push({
      runId: run.run_id,
      status: run.status,
      conversationId: run.conversation_id,
      messageId: run.message_id,
    });
  }
  return rows.sort((a, b) => b.runId.localeCompare(a.runId));
}

export interface TasksView {
  root: HTMLElement;
  render(): void;
}

export function mountTasksView(): TasksView {
  const root = h("div", { class: "tasks-surface" });
  const header = h("h2", {}, "Tasks");
  const rowsDiv = h("div", { class: "task-rows" });
  const content = h("div", { class: "tasks-content" });

  const graphWrapper = h("div", { class: "graph-wrapper" });
  const receiptsWrapper = h("div", { class: "receipts-wrapper" });
  const terminalWrapper = h("div", { class: "terminal-wrapper" });

  const graph = renderGraph(h("div"), graphWrapper, rowsDiv, graphWrapper, app.runs);
  const receipts = renderReceipts(h("div"), receiptsWrapper, rowsDiv, graphWrapper);
  const terminal = mountTerminalView(h("div"), terminalWrapper, rowsDiv, graphWrapper);

  content.append(header, rowsDiv, graph, receipts, terminal);
  root.append(content);

  return {
    root,
    render() {
      rowsDiv.replaceChildren();
      graph.render();
      receipts.render?.();
      terminal.render?.();
    },
  };
}