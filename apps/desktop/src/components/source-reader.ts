// Source revision reader: shows the exact pinned revision of an ingested
// source and renders its content. Content is untrusted: it is rendered as
// escaped text only (no Markdown parsing here — the reader is a byte-faithful
// view of the exact revision). Deletion flows through the P4 deletion
// commands and renders the returned `DeletionReceipt` when available.
import { datetimeText, h, kvRow, note, pill } from "./dom";

/** One source row in the knowledge list. */
export function sourceRow(source: SourceRevision): HTMLElement {
  return h("div", { class: "source-revision" },
    kvRow("source", source.source_id),
    kvRow("revision", source.digest),
    kvRow("mime", source.mime),
    kvRow("extractor", `${source.extractor_id} v${source.extractor_version}`),
    kvRow("locators", String(source.locators)),
    kvRow("ingested", datetimeText(source.created_at)),
  );
}

/** Reader panel for one revision's content. */
export function sourceReaderPanel(content: string | undefined, missingReason?: string): HTMLElement {
  const panel = h("div");
  if (content === undefined) {
    panel.append(note(missingReason ?? "The core did not return content for this revision.", "warn"));
    return panel;
  }
  const box = h("div", { class: "source-reader-text" });
  box.append(h("span", {}, content));
  panel.append(box);
  return panel;
}

/** Deletion receipt panel: what was actually removed, per the core. */
export function deletionReceiptPanel(receipt: DeletionReceipt | undefined, error?: string): HTMLElement {
  const panel = h("div", { class: "panel" });
  panel.append(h("h2", {}, "Deletion receipt"));
  if (error) {
    panel.append(note(error, "err"));
    return panel;
  }
  if (!receipt) {
    panel.append(note("The core returned no receipt.", "warn"));
    return panel;
  }
  const dl = h("div", {},
    kvRow("source", receipt.source_id),
    kvRow("chunks removed", String(receipt.chunks_removed)),
    kvRow("vectors removed", String(receipt.vectors_removed)),
    kvRow("revisions removed", String(receipt.revisions_removed)),
    kvRow("applied", datetimeText(receipt.applied_at)),
  );
  panel.append(dl, h("p", { class: "note" }, "Deleted content can no longer be cited; historic citations resolve to the deleted state."));
  return panel;
}

/** Embedding qualification state for the knowledge surface. */
export function embeddingStatePanel(state: { qualified: boolean; model?: string; dimensions?: number; note?: string }): HTMLElement {
  const panel = h("div", { class: "panel" });
  panel.append(h("h2", {}, "Embedding qualification"));
  if (!state.qualified) {
    panel.append(pill("unqualified", "embedding unqualified"));
    panel.append(note(state.note ?? "No qualified local embedding model is bound to this project. Retrieval falls back to the core's lexical pass; vector retrieval is off, not fake.", "info"));
    return panel;
  }
  panel.append(pill("qualified", "embedding qualified"));
  panel.append(h("div", {},
    kvRow("model", state.model ?? "unknown"),
    kvRow("dimensions", state.dimensions !== undefined ? String(state.dimensions) : "unknown"),
  ));
  return panel;
}
