// Diff rendering for approval details and review surfaces. The diff content
// is untrusted text: every line is inserted through the DOM builder's text
// nodes only, so a payload containing markup cannot inject DOM.
// Unified-diff syntax is recognised to colour additions/deletions/hunks.
import { h } from "./dom";

export interface DiffInput {
  path: string;
  before?: string;
  after?: string;
  unified?: string;
}

/** Renders a diff panel for one file. When both sides are present a simple
 * line diff is computed; otherwise the unified text is shown as-is. */
export function renderDiff(root: HTMLElement, diff: DiffInput): void {
  root.replaceChildren();
  const box = h("div", { class: "diff", "aria-label": `Diff for ${diff.path}` });
  box.append(h("div", { class: "diff-hunk" }, diff.path));
  if (diff.before !== undefined && diff.after !== undefined) {
    for (const line of lineDiff(diff.before, diff.after)) {
      const cls = line.op === "add" ? "diff-add" : line.op === "del" ? "diff-del" : "";
      const prefix = line.op === "add" ? "+" : line.op === "del" ? "-" : " ";
      box.append(h("span", { class: `diff-line ${cls}` }, `${prefix}${line.text}`));
    }
  } else if (diff.unified !== undefined) {
    if (diff.unified.trim() === "") {
      box.append(h("div", { class: "diff-empty" }, "No content change reported for this target."));
    }
    for (const raw of diff.unified.split("\n")) {
      const cls = raw.startsWith("+") ? "diff-add" : raw.startsWith("-") ? "diff-del" : raw.startsWith("@@") ? "diff-hunk" : "";
      box.append(h("span", { class: `diff-line ${cls}` }, raw));
    }
  } else {
    box.append(h("div", { class: "diff-empty" }, "The core did not report a diff for this action. The approval still shows its exact target and snapshot digest."));
  }
  root.append(box);
}

interface DiffLine {
  op: "add" | "del" | "same";
  text: string;
}

/** Minimal LCS-free line diff (common prefix/suffix + middle as del/add).
 * Good enough to display the change truthfully; the digest remains the
 * authoritative identifier. */
function lineDiff(before: string, after: string): DiffLine[] {
  const a = before.split("\n");
  const b = after.split("\n");
  const out: DiffLine[] = [];
  let startA = 0;
  let startB = 0;
  while (startA < a.length && startB < b.length && a[startA] === b[startB]) {
    out.push({ op: "same", text: a[startA] });
    startA += 1;
    startB += 1;
  }
  let endA = a.length;
  let endB = b.length;
  while (endA > startA && endB > startB && a[endA - 1] === b[endB - 1]) {
    endA -= 1;
    endB -= 1;
  }
  for (let i = startA; i < endA; i += 1) out.push({ op: "del", text: a[i] });
  for (let i = startB; i < endB; i += 1) out.push({ op: "add", text: b[i] });
  for (let i = endA; i < a.length; i += 1) out.push({ op: "same", text: a[i] });
  return out;
}
