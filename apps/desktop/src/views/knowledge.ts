// D3 Knowledge surface: source revisions, ingestion, retrieval search,
// deletion receipts, and approved-memory review/edit/forget. Embedding
// qualification state is shown truthfully (unqualified means vector retrieval
// is off, not fake). Citations resolve through the P3 resolver command.
import type { DeletionReceipt, SourceRevision, Citation, CitedExcerpt, CitationView, EmbeddingIndexReceipt, EmbeddingModelIdentity, MemoryDecision, MemoryRecord, QualificationRecord } from "@ironbark/contracts";
import { app, coreErrorNote, getClient, recordError } from "../app";
import { datetimeText, digestView, h, kvRow, note, pill } from "../components/dom";
import { renderCitationView } from "../components/citations";
import { sourceReaderPanel, deletionReceiptPanel, embeddingStatePanel } from "../components/source-reader";
import { memoryRow, type MemoryHandlers } from "../components/memory-review";

export interface KnowledgeView {
  root: HTMLElement;
  render(): void;
}

export function mountKnowledgeView(): KnowledgeView {
  const root = h("div", { class: "surface knowledge-surface" });
  const header = h("div", { class: "surface-header" });
  const body = h("div", { class: "surface-body" });
  root.append(header, body);

  let sources: SourceRevision[] = [];
  let revisions: SourceRevision[] = [];
  let currentSourceId: string | undefined;
  let memoryRecords: MemoryRecord[] = [];

  function render(): void {
    renderHeader();
    renderBody();
  }

  function renderHeader(): void {
    header.replaceChildren(
      h("div", { class: "title" }, "Knowledge"),
      h("div", { class: "subtitle" }, "Sources, retrieval, memory and embedding state"),
    );
  }

  async function renderBody(): Promise<void> {
    body.replaceChildren();
    if (!selectedProject()) {
      body.append(note("Select a project first (sidebar → project selector).", "info"));
      return;
    }
    await Promise.all([refreshSources(), refreshMemory()]);
    renderSources();
    renderRevisions();
    renderRetrieval();
    renderEmbeddingState();
    renderMemory();
  }

  function renderSources(): void {
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, "Sources"));
    if (sources.length === 0) {
      panel.append(note("No sources ingested. Use the form below to add a file path.", "info"));
    }
    for (const source of sources) {
      const row = h("div", { class: "source-row" }, sourceRow(source));
      row.append(
        h("button", { class: "subtle", type: "button" }, "Revisions").addEventListener("click", () => {
          currentSourceId = source.source_id;
          void refreshRevisions(source.source_id);
        }),
        h("button", { class: "danger", type: "button" }, "Delete").addEventListener("click", async () => {
          if (!window.confirm(`Delete source ${source.source_id}? All chunks and vectors will be removed.`)) return;
          try {
            const receipt = await getClient().command<DeletionReceipt>({ type: "source_delete", project_id: selectedProject()!.id, source_id: source.source_id });
            await refreshSources();
            body.append(deletionReceiptPanel(receipt));
          } catch (err) {
            recordError("source_delete", err);
            body.append(note(`Delete failed (core ${coreErrorNote(err)}): ${err instanceof Error ? err.message : String(err)}`, "err"));
          }
        }),
      );
      panel.append(row);
    }
    // Ingest form
    const form = h("div", { style: "margin-top:12px;padding:8px;border-top:1px solid var(--border)" });
    const pathInput = h("input", { type: "text", placeholder: "File path to ingest", style: "flex:1;padding:4px 8px;" });
    const submit = h("button", { class: "primary", type: "button" }, "Ingest");
    submit.addEventListener("click", async () => {
      const path = pathInput.value.trim();
      if (!path) return;
      try {
        const result = await getClient().command<SourceRevision>({ type: "source_ingest", project_id: selectedProject()!.id, source: { type: "file", path } });
        pathInput.value = "";
        await refreshSources();
        body.append(note(`Ingested ${result.source_id} (${result.digest.slice(0, 12)}…)`, ""));
      } catch (err) {
        recordError("source_ingest", err);
        body.append(coreErrorNote(err));
      }
    });
    form.append(pathInput, submit);
    panel.append(form);
    body.append(panel);
  }

  function renderRevisions(): void {
    if (!currentSourceId) return;
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, `Revisions for ${currentSourceId}`),
      h("button", { class: "subtle", type: "button" }, "Close").addEventListener("click", () => {
        currentSourceId = undefined;
        render();
      }));
    if (revisions.length === 0) {
      panel.append(note("No revisions recorded for this source.", "info"));
    }
    for (const rev of revisions) {
      panel.append(sourceReaderPanel(undefined, `Content for revision ${rev.digest.slice(0, 12)}… is opened via citation resolve in the conversation. Use the citation link in a message to read the exact locator.`));
    }
    body.append(panel);
  }

  function renderRetrieval(): void {
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, "Retrieval search"));
    const input = h("input", { type: "text", placeholder: "Query", style: "flex:1;padding:4px 8px;" });
    const limit = h("input", { type: "number", value: "10", min: "1", max: "100", style: "width:60px;padding:4px 8px;" });
    const btn = h("button", { class: "primary", type: "button" }, "Search");
    btn.addEventListener("click", async () => {
      const query = input.value.trim();
      if (!query) return;
      try {
        const results = await getClient().command<CitedExcerpt[]>({ type: "retrieval_search", project_id: selectedProject()!.id, query, limit: Number(limit.value) });
        renderRetrievalResults(panel, results);
      } catch (err) {
        recordError("retrieval_search", err);
        panel.append(coreErrorNote(err));
      }
    });
    panel.append(h("div", { style: "display:flex;gap:6px;margin-bottom:8px;" }, input, limit, btn));
    const resultsSlot = h("div", { class: "retrieval-results" });
    panel.append(resultsSlot);
    body.append(panel);
  }

  function renderRetrievalResults(panel: HTMLElement, results: CitedExcerpt[]): void {
    const slot = panel.querySelector(".retrieval-results") as HTMLElement;
    slot.replaceChildren();
    if (results.length === 0) {
      slot.append(note("No results.", "info"));
      return;
    }
    for (const excerpt of results) {
      const card = h("div", { class: "retrieval-card" });
      card.append(h("div", {}, kvRow("source", excerpt.citation.source_id), kvRow("revision", digestView(excerpt.citation.revision_digest))));
      card.append(h("div", { class: "excerpt" }, excerpt.text.slice(0, 400)));
      card.append(h("button", { class: "subtle", type: "button" }, "Open citation").addEventListener("click", async () => {
        const view = await getClient().command<CitationView>({ type: "citation_resolve", project_id: selectedProject()!.id, citation: excerpt.citation });
        renderCitationView(view, excerpt.citation, slot);
      }));
      slot.append(card);
    }
  }

  function renderEmbeddingState(): void {
    body.append(embeddingStatePanel({ qualified: false, note: "Embedding qualification state is not yet wired; vector retrieval falls back to the core's lexical pass." }));
  }

  async function renderMemory(): Promise<void> {
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, "Approved memory"));
    // Propose box
    const propose = h("div", { style: "margin-bottom:12px;padding:8px;border-bottom:1px solid var(--border)" });
    const textArea = h("textarea", { placeholder: "Memory text", rows: "3", style: "width:100%;margin-bottom:6px;" });
    const provenance = h("input", { type: "text", placeholder: "Provenance (e.g. user feedback, conversation summary)", style: "width:100%;margin-bottom:6px;" });
    const proposeBtn = h("button", { class: "primary", type: "button" }, "Propose");
    proposeBtn.addEventListener("click", async () => {
      const text = textArea.value.trim();
      const prov = provenance.value.trim() || "manual";
      if (!text) return;
      try {
        await getClient().command<{ id: string }>({ type: "memory_propose", project_id: selectedProject()!.id, text, provenance: prov });
        textArea.value = "";
        provenance.value = "";
        await refreshMemory();
      } catch (err) {
        recordError("memory_propose", err);
        panel.append(coreErrorNote(err));
      }
    });
    propose.append(textArea, provenance, proposeBtn);
    panel.append(propose);
    // List
    const list = h("div", { class: "memory-list" });
    for (const record of memoryRecords) {
      list.append(memoryRow(record, {
        onPropose: async () => {}, // not used here; propose uses memory_propose
        onDecide: async (id, decision, editedText) => {
          await getClient().command<unknown>({ type: "memory_decide", memory_id: id, decision, ...(editedText ? { text: editedText } : {}) });
          await refreshMemory();
        },
        onForget: async (id) => {
          await getClient().command<unknown>({ type: "memory_forget", memory_id: id });
          await refreshMemory();
        },
      }));
    }
    panel.append(list);
    body.append(panel);
  }

  async function refreshSources(): Promise<void> {
    try {
      sources = await getClient().command<SourceRevision[]>({ type: "sources_list", project_id: selectedProject()!.id });
    } catch (err) {
      recordError("sources_list", err);
      sources = [];
    }
  }

  async function refreshRevisions(sourceId: string): Promise<void> {
    try {
      revisions = await getClient().command<SourceRevision[]>({ type: "source_revisions", source_id: sourceId });
    } catch (err) {
      recordError("source_revisions", err);
      revisions = [];
    }
    render();
  }

  async function refreshMemory(): Promise<void> {
    try {
      memoryRecords = await getClient().command<MemoryRecord[]>({ type: "memory_list", project_id: selectedProject()!.id });
    } catch (err) {
      recordError("memory_list", err);
      memoryRecords = [];
    }
  }

  return { root, render };
}

function selectedProject() {
  return app.projects.find((p) => p.id === app.selectedProjectId) ?? app.projects[0];
}