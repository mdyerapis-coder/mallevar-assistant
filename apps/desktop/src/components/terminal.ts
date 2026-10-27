// Terminal surface for a run. `TerminalView.attach(run_id)` reads the owned
// PTY stream — the stream itself lives natively (the webview never launches a
// shell). The webview renders whatever the core has durably recorded for the
// run: terminal output receipts in the run's event trail. When the owned
// stream is not exposed in this build, the surface says so plainly instead
// of showing an invented prompt.
import type { EventEnvelope } from "../core-client";
import { h, note } from "./dom";

export interface TerminalLine {
  text: string;
  kind: "out" | "err";
}

/** Extracts terminal lines from a run's durable receipts. Lines carry no
 * markup; they are rendered as escaped text. */
export function terminalLinesFromReceipts(receipts: EventEnvelope[]): TerminalLine[] {
  const out: TerminalLine[] = [];
  for (const envelope of receipts) {
    const payload = envelope.payload;
    if (payload.type !== "terminal_output") continue;
    const p = payload as { text?: unknown; stream?: unknown };
    if (typeof p.text === "string" && p.text !== "") {
      for (const line of p.text.split("\n")) {
        if (line !== "") out.push({ text: line, kind: p.stream === "stderr" ? "err" : "out" });
      }
    }
  }
  return out;
}

export interface TerminalViewHandle {
  root: HTMLElement;
  attach(runId: string): void;
  setLines(lines: TerminalLine[], note?: string): void;
}

export function mountTerminalView(): TerminalViewHandle {
  const root = h("div", { class: "terminal-wrap" });
  const box = h("div", { class: "terminal", "aria-label": "Owned terminal output", tabIndex: 0 });
  const noteEl = h("div", { class: "terminal-note" });
  root.append(box, noteEl);

  function attach(runId: string): void {
    box.setAttribute("data-run", runId);
    box.replaceChildren(h("span", { class: "term-mute" }, `attaching to owned stream for run ${runId.slice(0, 8)}…`));
    noteEl.replaceChildren();
  }

  function setLines(lines: TerminalLine[], note?: string): void {
    box.replaceChildren();
    if (lines.length === 0) {
      box.append(h("span", { class: "term-mute" }, "no terminal output recorded for this run"));
    }
    for (const line of lines) {
      box.append(h("span", { class: line.kind === "err" ? "term-err" : "" }, `${line.text}\n`));
    }
    box.scrollTop = box.scrollHeight;
    noteEl.replaceChildren(note ? note : "Terminal output is the run's owned stream as recorded by the core; the webview never launches a shell.");
  }

  return { root, attach, setLines };
}
