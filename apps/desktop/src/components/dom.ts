// Small DOM builder shared by the D2/D3 views. All untrusted text (model
// output, source excerpts, tool receipts, error messages) is inserted only
// through the builder's text nodes: the webview never inserts raw HTML from
// content, so untrusted source/tool text cannot inject markup, invoke IPC or
// drive the UI. `h` sets attributes explicitly and never parses an HTML
// string.
export function h(
  tag: string,
  attrs: Record<string, string | number | boolean> = {},
  ...children: Array<Node | string | number>
): HTMLElement {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === false || value === null || value === undefined) continue;
    if (key === "class") el.className = String(value);
    else if (key === "text") el.textContent = String(value);
    else if (value === true) el.setAttribute(key, "");
    else el.setAttribute(key, String(value));
  }
  for (const child of children) {
    if (child === null || child === undefined) continue;
    el.append(typeof child === "string" || typeof child === "number" ? document.createTextNode(String(child)) : child);
  }
  return el;
}

export function escapeHtml(text: string): string {
  return text
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

/** A run-state / availability pill from the token palette. */
export function pill(state: string, label?: string): HTMLElement {
  const known = [
    "queued", "ready", "running", "awaiting_approval", "blocked", "reviewing",
    "verifying", "paused", "cancelling", "cancelled", "succeeded", "failed",
    "unknown", "unavailable", "unqualified", "qualified", "ok", "error", "denied",
    "executing", "uncertain", "abstain", "invalid", "available", "enabled", "deleted",
  ];
  const cls = known.includes(state) ? state : "unknown";
  return h("span", { class: `pill ${cls}` }, h("span", { class: "dot" }), label ?? state.replaceAll("_", " "));
}

export function note(text: string, kind: "" | "warn" | "err" | "info" = ""): HTMLElement {
  return h("p", { class: `note ${kind}`.trim() }, text);
}

/** Compact digest rendering: first/last 12 hex chars, full value in the
 * title for keyboard/screen-reader access. */
export function digestView(digest: string): HTMLElement {
  const short = digest.length > 24 ? `${digest.slice(0, 12)}…${digest.slice(-12)}` : digest;
  return h("code", { class: "inline", title: digest }, short);
}

export function dateTimeText(ms: number): string {
  try {
    return new Date(ms).toLocaleString("en-AU", { dateStyle: "medium", timeStyle: "short" });
  } catch {
    return String(ms);
  }
}

/** One key/value row for the `.kv` layout. */
export function kvRow(key: string, value: string | HTMLElement): HTMLElement {
  const valueEl = typeof value === "string" ? h("span", { class: "kv-value" }, value) : value;
  return h("div", { class: "kv" }, h("span", { class: "kv-key" }, key), valueEl);
}

/** Human-readable form of a JSON value, truncated so receipts stay readable. */
export function jsonSummary(value: unknown, max = 400): string {
  try {
    const text = JSON.stringify(value);
    if (text === undefined) return String(value);
    return text.length > max ? `${text.slice(0, max)}…` : text;
  } catch {
    return String(value);
  }
}
