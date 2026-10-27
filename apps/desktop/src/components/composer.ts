// Persistent conversation composer. Model/provider and harness are SEPARATE
// selectors (the design constraint); Chat advertises no executable tools.
//
// Send semantics: the composer mints one request id per draft and passes it
// to `conversation_queue_turn` as the durable frame identity. Repeated Send
// of an in-flight draft reuses the same request id, so the core de-duplicates
// it into the original run instead of double-dispatching. Stop sends
// `run_interrupt` for the run of the active branch; the core reports its real
// unsupported state until C4 wires it, and the composer shows that state.
//
// Unsupported route selections display their reason inline — the option is
// still selectable (the core's typed refusal is the source of truth), but a
// `reason-note` explains why the route cannot be used.
//
// The composer is mounted ONCE per conversation branch. Redraws call the
// in-place `set*` updates (routes, attachments, active run) so the user's
// draft and focus survive live event streams.
import type { ContentPart, ContextPackage, ConnectionDescriptor, HarnessKind, RoutePin } from "@ironbark/contracts";
import { CoreError, type CoreClient } from "../core-client";
import { h } from "./dom";
import type { AttachmentRow } from "./attachments";

export interface RouteOption {
  connection: ConnectionDescriptor;
  model: { id: string; context_limit: number; output_limit: number };
}

export interface ComposerDraft {
  conversation_id: string;
  branch_id: string;
  text: string;
  parts: ContentPart[];
  pin: RoutePin;
  context: ContextPackage | undefined;
  requestId: string;
}

export interface ComposerConfig {
  client: CoreClient;
  conversationId: string;
  branchId: string;
  routes: RouteOption[];
  /** Routes the project policy refuses, with the reason shown in the UI. */
  blocked: Record<string, string>;
  harnesses: string[];
  attachments: AttachmentRow[];
  context: ContextPackage | undefined;
  activeRunId?: string;
  onSubmit: (draft: ComposerDraft) => Promise<void>;
  onStop: (runId: string) => Promise<void>;
}

export interface ComposerHandle {
  root: HTMLElement;
  focus: () => void;
  setText: (text: string) => void;
  /** Re-runs the last send with the retained request id (repeat Send). */
  resend: () => Promise<void>;
  /** In-place updates so the draft and focus survive redraws. */
  setRoutes: (routes: RouteOption[], blocked: Record<string, string>, harnesses: string[]) => void;
  setAttachments: (rows: AttachmentRow[]) => void;
  setActiveRun: (runId: string | undefined) => void;
}

const HARNESS_KINDS = new Set<string>(["omp", "codex", "opencode", "claude", "hermes", "alphacode"]);

export function mountComposer(config: ComposerConfig): ComposerHandle {
  const root = h("div", { class: "composer" });
  const inner = h("div", { class: "composer-inner" });

  // Route selector: model/provider.
  const routeSelect = h("select", { class: "control-select", "aria-label": "Model and provider" });
  const harnessSelect = h("select", { class: "control-select", "aria-label": "Harness" });
  const reasonNote = h("div", { class: "reason-note", hidden: true });

  let routes: RouteOption[] = config.routes;
  let blocked: Record<string, string> = config.blocked;

  const routeId = (option: RouteOption): string => `${option.connection.id}/${option.model.id}`;

  function renderRoutes(): void {
    routeSelect.replaceChildren(h("option", { value: "" }, "Select a model…"));
    for (const option of routes) {
      routeSelect.append(h("option", { value: routeId(option) }, `${option.model.id} · ${option.connection.protocol} (${option.connection.id})`));
    }
    updateReason();
  }

  function currentPin(): RoutePin | undefined {
    const selected = routeSelect.selectedOptions[0];
    if (!selected || selected.value === "") return undefined;
    const option = routes.find((r) => routeId(r) === selected.value);
    if (!option) return undefined;
    const harnessValue = harnessSelect.value;
    const harness = harnessValue !== "" && HARNESS_KINDS.has(harnessValue) ? (harnessValue as HarnessKind) : undefined;
    return { connection_id: option.connection.id, model_id: option.model.id, ...(harness ? { harness } : {}) };
  }

  function updateReason(): void {
    const selected = routeSelect.selectedOptions[0];
    const reason = selected ? blocked[selected.value] : undefined;
    if (reason) {
      reasonNote.replaceChildren(reason);
      reasonNote.hidden = false;
    } else {
      reasonNote.hidden = true;
    }
  }

  routeSelect.addEventListener("change", updateReason);

  // Harness selector: engine identity only (never folds into the model pick).
  function renderHarnesses(): void {
    harnessSelect.replaceChildren(h("option", { value: "" }, "Harness: none (chat only)"));
    for (const kind of config.harnesses) harnessSelect.append(h("option", { value: kind }, kind));
  }

  const textarea = h("textarea", {
    placeholder: "Message the assistant. Chat only — no executable tools.",
    "aria-label": "Message",
    rows: "2",
  });

  // Attachment provenance rows (digest, origin, ingestion progress).
  const attachmentBox = h("div", { class: "composer-attachments", style: "display:flex;flex-direction:column;gap:6px;" });
  function renderAttachments(): void {
    attachmentBox.replaceChildren(...config.attachments.map((row) => row.node));
  }

  const sendButton = h("button", { class: "primary", type: "button" }, "Send");
  const stopButton = h("button", { type: "button", disabled: !config.activeRunId }, "Stop");
  const sendStatus = h("div", { class: "composer-status" });

  const tooling = h("div", { class: "composer-toolbar" },
    routeSelect,
    harnessSelect,
    h("span", { class: "muted" }, "Chat only · no executable tools"),
    h("span", { class: "spacer" }),
    stopButton,
    sendButton,
  );

  const inputs = h("div", { class: "composer-inputs" }, attachmentBox, textarea, reasonNote, tooling);
  inner.append(inputs, sendStatus);
  root.append(inner);
  renderRoutes();
  renderHarnesses();
  renderAttachments();

  // One request identity per draft; repeat Send retains it.
  let pendingRequestId = crypto.randomUUID();
  let lastDraft: ComposerDraft | undefined;
  let busy = false;

  function buildParts(): ContentPart[] {
    const text = textarea.value;
    const parts: ContentPart[] = [];
    for (const attachment of config.attachments) {
      parts.push({ type: "artifact", artifact_id: attachment.artifactId });
    }
    if (text.trim() !== "") parts.push({ type: "text", text });
    return parts;
  }

  async function send(reuseRequestId: boolean): Promise<void> {
    if (busy) return;
    const pin = currentPin();
    if (!pin) {
      sendStatus.className = "composer-status error";
      sendStatus.replaceChildren("Select a model and provider first.");
      routeSelect.focus();
      return;
    }
    const parts = buildParts();
    if (parts.length === 0) {
      sendStatus.className = "composer-status error";
      sendStatus.replaceChildren("Nothing to send: add text or an attached source.");
      return;
    }
    const draft: ComposerDraft = {
      conversation_id: config.conversationId,
      branch_id: config.branchId,
      text: textarea.value,
      parts,
      pin,
      context: config.context,
      requestId: reuseRequestId ? pendingRequestId : (pendingRequestId = crypto.randomUUID()),
    };
    lastDraft = draft;
    busy = true;
    sendButton.disabled = true;
    sendStatus.className = "composer-status";
    sendStatus.replaceChildren(`Sending as request ${draft.requestId.slice(0, 8)}…`);
    try {
      await config.onSubmit(draft);
      sendStatus.replaceChildren(`Queued. Request identity retained: ${draft.requestId.slice(0, 8)}`);
    } catch (err) {
      sendStatus.className = "composer-status error";
      const code = err instanceof CoreError ? err.code : "transport";
      sendStatus.replaceChildren(`Send failed (core ${code}): ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      busy = false;
      sendButton.disabled = false;
    }
  }

  sendButton.addEventListener("click", () => void send(true));
  textarea.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      void send(true);
    }
  });

  stopButton.addEventListener("click", async () => {
    const runId = config.activeRunId;
    if (!runId) return;
    stopButton.disabled = true;
    sendStatus.className = "composer-status";
    sendStatus.replaceChildren(`Interrupting run ${runId.slice(0, 8)}…`);
    try {
      await config.onStop(runId);
      sendStatus.replaceChildren("Interrupt requested; the core commits the observed terminal state.");
    } catch (err) {
      sendStatus.className = "composer-status error";
      const code = err instanceof CoreError ? err.code : "transport";
      sendStatus.replaceChildren(`Stop not performed (core ${code}): ${err instanceof Error ? err.message : String(err)}.`);
    }
  });

  return {
    root,
    focus: () => textarea.focus(),
    setText: (text: string) => {
      textarea.value = text;
      textarea.focus();
    },
    resend: async () => {
      if (lastDraft) {
        pendingRequestId = lastDraft.requestId;
        textarea.value = lastDraft.text;
        await send(true);
      }
    },
    setRoutes: (nextRoutes, nextBlocked, harnesses) => {
      routes = nextRoutes;
      blocked = nextBlocked;
      renderRoutes();
      config.harnesses = harnesses;
      renderHarnesses();
    },
    setAttachments: (rows) => {
      config.attachments = rows;
      renderAttachments();
    },
    setActiveRun: (runId) => {
      config.activeRunId = runId;
      stopButton.disabled = runId === undefined;
    },
  };
}
