// Tool/approval receipt rendering. Receipts are durable core events
// (`tool_started`, `tool_finished`, `approval_needed`, `error`,
// `usage_observed`, `turn_finished`) projected into `ReceiptLine` records.
// Unknown or uncertain states stay labelled — the UI never upgrades an
// unobserved outcome to success.
import { datetimeText, h, jsonSummary, note, pill } from "./dom";

export interface ReceiptLine {
  kind: string;
  state: string;
  at: number;
  detail?: string;
}

/** Renders a list of receipt lines. Empty lists say so plainly. */
export function renderReceipts(root: HTMLElement, lines: ReceiptLine[]): void {
  root.replaceChildren();
  if (lines.length === 0) {
    root.append(note("No receipts recorded.", "info"));
    return;
  }
  const box = h("div", { class: "receipts" });
  for (const line of lines) box.append(receiptNode(line));
  root.append(box);
}

export function receiptNode(line: ReceiptLine): HTMLElement {
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

/** A receipt whose recorded outcome is unknown/uncertain, for truthful
 * labelling. */
export function hasUnknownUsage(lines: ReceiptLine[]): boolean {
  return lines.some((line) => line.state === "uncertain" || line.state === "unknown");
}

/** One-line JSON summary used inside receipt details. */
export function summary(value: unknown, max = 160): string {
  return jsonSummary(value, max);
}
