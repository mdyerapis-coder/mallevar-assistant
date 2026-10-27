// Exact citation rendering and opening. `CitationLink.open(citation)` is the
// D2 interface: it invokes the P3 knowledge resolver command
// (`citation_resolve`) and renders the returned `CitationView` — content is
// shown only for the exact pinned source revision/locator the core returns.
// `deleted`/`missing` views render their real state; no cached or guessed
// text is substituted.
import type { Citation, CitationView, ProjectId } from "@ironbark/contracts";
import type { CoreClient, EventEnvelope } from "../core-client";
import { renderMarkdown } from "./markdown";

export interface CitationOpener {
  client: CoreClient;
  project: ProjectId;
}

/** Inline citation reference rendered inside conversation text. */
export function citationLink(opener: CitationOpener, citation: Citation, index: number): HTMLElement {
  const link = h(
    "button",
    { class: "citation-link", title: `Open citation ${index + 1}: source ${citation.source_id}`, type: "button" },
    h("span", { class: "citation-ref" }, `[${index + 1}]`),
  );
  link.addEventListener("click", async () => {
    await CitationLink.open(opener, citation);
  });
  return link;
}

export class CitationLink {
  /** Resolves and renders one exact citation. Returns the view so a caller
   * can assert the resolved state in the smoke checklist. */
  static async open(opener: CitationOpener, citation: Citation): Promise<CitationView> {
    const view = await opener.client.command<CitationView>({
      type: "citation_resolve",
      project_id: opener.project,
      citation,
    });
    renderCitationView(view, citation);
    return view;
  }
}

/** Renders a resolved `CitationView` into `panel`. */
export function renderCitationView(view: CitationView, citation: Citation, panel: HTMLElement): void {
  panel.replaceChildren();
  const head = h("div", { class: "citation-panel-head" },
    h("span", {}, "source ", h("code", { class: "inline" }, citation.source_id)),
    h("span", {}, "revision ", digestView(citation.revision_digest)),
    locatorText(citation),
  );
  const body = h("div", { class: "citation-text" });
  if (view.type === "content") {
    const md = h("div", { class: "md" });
    renderMarkdown(md, view.text);
    body.append(md);
  } else if (view.type === "deleted") {
    body.append(note("This source revision has been deleted. The citation cannot be opened.", "warn"));
    panel.classList.add("deleted");
  } else {
    body.append(note("The cited revision or locator is no longer available in this project.", "warn"));
    panel.classList.add("missing");
  }
  panel.append(head, body);
}

function locatorText(citation: Citation): HTMLElement {
  const { locator } = citation;
  const bits: string[] = [];
  if (locator.line_start !== undefined) bits.push(`lines ${locator.line_start}${locator.line_end !== undefined ? `–${locator.line_end}` : ""}`);
  if (locator.section) bits.push(`section ${locator.section}`);
  if (locator.page !== undefined) bits.push(`page ${locator.page}`);
  if (locator.sheet) bits.push(`sheet ${locator.sheet}${locator.cell_range ? `!${locator.cell_range}` : ""}`);
  if (locator.time_start_ms !== undefined) bits.push(`t+${(locator.time_start_ms / 1000).toFixed(1)}s`);
  return h("span", {}, bits.join(" · "));
}

/** Renders the citation excerpts of a turn snapshot context inline after the
 * assistant message body: each excerpt carries its exact citation, and the
 * link opens it through the resolver. */
export function renderContextCitations(opener: CitationOpener, excerpts: Array<{ text: string; citation: Citation }>): HTMLElement | null {
  if (excerpts.length === 0) return null;
  const wrap = h("div", { class: "turn-citations" });
  excerpts.forEach((excerpt, index) => {
    const item = h("div", { class: "attachment-row" });
    item.append(h("span", { class: "muted" }, "cited"), citationLink(opener, excerpt.citation, index));
    const md = h("div", { class: "md" });
    renderMarkdown(md, excerpt.text);
    item.append(md);
    wrap.append(item);
  });
  return wrap;
}

/** Extracts citation excerpts from a message's durable receipts (the
 * provider_message receipts carry the pinned route; the turn snapshot in
 * history carries the context). Used by the conversation view to keep
 * citation links exact without re-deriving content. */
export function citationsFromReceipts(receipts: EventEnvelope[]): Array<{ text: string; citation: Citation }> {
  const out: Array<{ text: string; citation: Citation }> = [];
  for (const envelope of receipts) {
    const payload = envelope.payload as { type: string; [key: string]: unknown };
    if (payload.type !== "provider_message") continue;
    collectCitations(payload.content, out);
  }
  return out;
}

function collectCitations(content: unknown, out: Array<{ text: string; citation: Citation }>): void {
  if (Array.isArray(content)) {
    for (const part of content) collectCitations(part, out);
    return;
  }
  if (content && typeof content === "object") {
    const record = content as Record<string, unknown>;
    const citation = record.citation;
    const text = record.text;
    if (citation && typeof text === "string") {
      const candidate = citation as Citation;
      if (candidate.source_id && candidate.revision_digest) {
        out.push({ text, citation: candidate });
      }
    }
    for (const value of Object.values(record)) collectCitations(value, out);
  }
}
