// Attachment ingestion with real provenance. The flow uses the core's wired
// C5 import commands only: `import_preview` reads the file natively and
// returns a digest-bound preview (path, digest, bytes, device, inode);
// `import_apply` commits it and returns the `ImportReceipt` carrying the
// durable `artifact_id`. The UI shows those real states as ingestion
// progress (preview → applying → applied) and never fabricates a receipt.
import type { ImportFormat, ImportPreview, ImportReceipt, ProjectId } from "@ironbark/contracts";
import type { CoreClient } from "../core-client";
import { CoreError } from "../core-client";
import { digestView, h, note } from "./dom";

export interface AttachmentRow {
  artifactId: string;
  label: string;
  digest: string;
  node: HTMLElement;
}

export interface AttachmentsHandle {
  node: HTMLElement;
  rows: () => AttachmentRow[];
  clear: () => void;
  onChange: (fn: (rows: AttachmentRow[]) => void) => void;
}

export function mountAttachments(client: CoreClient, project: ProjectId): AttachmentsHandle {
  const panel = h("div", { class: "attachments" });
  const pathInput = h("input", {
    placeholder: "/path/to/source (file on this workstation)",
    "aria-label": "Source file path",
    spellcheck: "false",
    style: "flex:1 1 auto;min-width:160px;",
  });
  const formatSelect = h("select", { "aria-label": "Import format" },
    h("option", { value: "document" }, "document"),
    h("option", { value: "native_transcript" }, "native transcript"),
  );
  const originInput = h("input", {
    placeholder: "transcript origin (harness/profile)",
    "aria-label": "Transcript origin",
    hidden: true,
    style: "flex:0 1 220px;",
  });
  const previewButton = h("button", { type: "button" }, "Preview");
  const applyButton = h("button", { type: "button", disabled: true }, "Apply");
  const stateNote = h("div", { class: "composer-status" });
  const previewBox = h("div");
  const rowBox = h("div", { style: "display:flex;flex-direction:column;gap:6px;margin-top:8px;" });

  formatSelect.addEventListener("change", () => {
    originInput.hidden = formatSelect.value !== "native_transcript";
  });

  const rows: AttachmentRow[] = [];
  let change: (rows: AttachmentRow[]) => void = () => {};
  let preview: ImportPreview | undefined;

  function emit(): void {
    change(rows);
  }

  function addRow(receipt: ImportReceipt): void {
    const label = receipt.source_id ?? receipt.artifact_id ?? receipt.connection_id ?? "imported source";
    const row = h("div", { class: "attachment-row" },
      h("span", {}, label),
      h("span", { class: "digest", title: receipt.source_digest }, `sha256 ${receipt.source_digest.slice(0, 16)}…`),
      h("span", { class: "progress" }, `artifact ${receipt.artifact_id ? receipt.artifact_id.slice(0, 8) : "none"}`),
    );
    rows.push({ artifactId: receipt.artifact_id ?? "", label, digest: receipt.source_digest, node: row });
    rowBox.append(row);
    emit();
  }

  previewButton.addEventListener("click", async () => {
    const path = pathInput.value.trim();
    if (path === "") {
      stateNote.className = "composer-status error";
      stateNote.replaceChildren("Enter a source file path to preview.");
      return;
    }
    const source: { path: string; format: ImportFormat } =
      formatSelect.value === "native_transcript"
        ? { path, format: { type: "native_transcript", origin: originInput.value.trim() || "unknown" } }
        : { path, format: { type: "document" } };
    previewButton.disabled = true;
    applyButton.disabled = true;
    previewBox.replaceChildren();
    stateNote.className = "composer-status";
    stateNote.replaceChildren("Previewing — the core reads the file and binds its digest…");
    try {
      preview = await client.command<ImportPreview>({ type: "import_preview", project_id: project, source });
      renderPreview(preview);
      stateNote.replaceChildren(`Preview bound (digest ${preview.digest.slice(0, 12)}…). Apply commits it to this project.`);
      applyButton.disabled = false;
    } catch (err) {
      preview = undefined;
      stateNote.className = "composer-status error";
      const code = err instanceof CoreError ? err.code : "transport";
      stateNote.replaceChildren(`Preview failed (core ${code}): ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      previewButton.disabled = false;
    }
  });

  function renderPreview(p: ImportPreview): void {
    const table = h("table", { class: "data-table" },
      h("thead", {}, h("tr", {}, h("th", {}, "file"), h("th", {}, "digest"), h("th", {}, "bytes"))),
    );
    const tbody = h("tbody");
    for (const file of p.files) {
      tbody.append(h("tr", {},
        h("td", { class: "mono" }, file.path),
        h("td", {}, digestView(file.digest)),
        h("td", {}, String(file.bytes)),
      ));
    }
    table.append(tbody);
    previewBox.replaceChildren(table);
    if (p.warnings.length > 0) {
      for (const warning of p.warnings) previewBox.append(note(warning, "warn"));
    }
  }

  applyButton.addEventListener("click", async () => {
    if (!preview) return;
    applyButton.disabled = true;
    stateNote.className = "composer-status";
    stateNote.replaceChildren("Applying preview — the core commits the objects…");
    try {
      const receipt = await client.command<ImportReceipt>({ type: "import_apply", project_id: project, preview_digest: preview.digest });
      addRow(receipt);
      preview = undefined;
      previewBox.replaceChildren();
      pathInput.value = "";
      stateNote.replaceChildren("Source committed. It can now be attached to a message or cited.");
    } catch (err) {
      stateNote.className = "composer-status error";
      const code = err instanceof CoreError ? err.code : "transport";
      stateNote.replaceChildren(`Apply failed (core ${code}): ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      applyButton.disabled = false;
    }
  });

  const header = h("div", { style: "display:flex;gap:8px;flex-wrap:wrap;align-items:center;" },
    h("span", { class: "muted" }, "Add source"),
    pathInput,
    formatSelect,
    originInput,
    previewButton,
    applyButton,
  );
  panel.append(header, stateNote, previewBox, rowBox);

  return {
    node: panel,
    rows: () => rows,
    clear: () => {
      rows.length = 0;
      rowBox.replaceChildren();
      emit();
    },
    onChange: (fn) => {
      change = fn;
    },
  };
}
