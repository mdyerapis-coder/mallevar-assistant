// D2 conversation workspace: readable conversation stream with the durable
// P2 commands — stream/partial/error via events, history/rename/archive/
// delete, edit (branches), regenerate, and the persistent composer.
//
// Every control acts through a core command; a control whose dispatch arm is
// not wired yet surfaces the core's real typed error instead of a fake
// success. Untrusted model/source text is rendered through the safe
// Markdown renderer only.
import nightArt from "../assets/ironbark-night.jpg";
import type {
  BranchView,
  ConversationHistory,
  ExportFormat,
  MessageView,
  QueuedTurn,
  RoutePin,
  RunResolution,
} from "@ironbark/contracts";
import { app, coreErrorNote, getClient, recordError, refreshConversation, refreshRecent, selectedProject } from "../app";
import { datetimeText, h, note, pill } from "../components/dom";
import { mountComposer, type ComposerHandle, type ComposerDraft } from "../components/composer";
import { mountAttachments, type AttachmentsHandle } from "../components/attachments";
import { renderMarkdown } from "../components/markdown";
import { renderContextCitations, type CitationOpener } from "../components/citations";

export interface ConversationView {
  root: HTMLElement;
  render(): void;
}

export function mountConversationView(): ConversationView {
  const root = h("div", { class: "conv-surface" });
  const header = h("div", { class: "surface-header" });
  const body = h("div", { class: "surface-body" });
  const composerSlot = h("div");
  let composer: ComposerHandle | undefined;
  let attachments: AttachmentsHandle | undefined;
  let composerConversationId: string | undefined;
  let composerBranchId: string | undefined;

  root.append(header, body, composerSlot);

  function opener(): CitationOpener | undefined {
    const project = selectedProject();
    if (!project) return undefined;
    return { client: getClient(), project: project.id };
  }


  function activeBranch(): { branch: BranchView; leafMessage: MessageView | undefined } | undefined {
    const history = app.history;
    if (!history) return undefined;
    // The active branch is the one with the newest leaf message; root is the
    // fallback when the conversation has a single branch.
    let best: BranchView | undefined;
    for (const branch of history.branches) {
      const leaf = branch.messages[branch.messages.length - 1];
      if (!leaf) continue;
      if (!best) {
        best = branch;
        continue;
      }
      const bestLeaf = best.messages[best.messages.length - 1];
      if (leaf && bestLeaf && leaf.created_at >= bestLeaf.created_at) best = branch;
    }
    if (!best) return undefined;
    const leafMessage = best.messages[best.messages.length - 1];
    return { branch: best, leafMessage };
  }

  function renderHeader(): void {
    const history = app.history;
    const project = selectedProject();
    const title = h("div", { class: "title" }, history?.title ?? "New conversation");
    const subtitle = h("div", { class: "subtitle" }, project ? `${project.name} · ${project.root}` : "no project selected");
    const actions = h("div", { class: "conv-actions", style: "display:flex;gap:6px;flex-wrap:wrap;" });
    if (history) {
      actions.append(
        headerButton("Rename", () => void rename(history.id)),
        headerButton("Archive", () => void archive(history.id)),
        headerButton("Delete", () => void del(history.id), { class: "danger" }),
        headerButton("Export", () => void exportMarkdown(history.id)),
      );
      if (history.archived) actions.append(pill("paused", "archived"));
    }
    header.replaceChildren(title, subtitle, h("span", { class: "spacer" }), actions);
  }

  function headerButton(label: string, onClick: () => void, attrs: Record<string, string> = {}): HTMLButtonElement {
    const button = h("button", { type: "button", ...attrs }, label);
    button.addEventListener("click", onClick);
    return button;
  }

  function renderWelcome(): void {
    const project = selectedProject();
    const art = h("img", { class: "welcome-art", src: nightArt, alt: "Ironbark night sky — approved artwork" });
    const copy = h("div", { class: "welcome-copy" },
      project
        ? "Start a conversation. Chat only: the model answers, nothing here executes tools. Attach real sources by path and cite exact revisions."
        : "Create or select a project first (Projects view). Empty state stays empty — no sample content.",
    );
    body.replaceChildren(h("div", { class: "narrow welcome" }, art, copy));
  }

  function messageBody(message: MessageView): HTMLElement {
    const wrap = h("div", { class: "turn-body" });
    for (const part of message.parts) {
      if (part.type === "text") {
        const md = h("div", { class: "md" });
        renderMarkdown(md, part.text);
        wrap.append(md);
      } else if (part.type === "artifact") {
        wrap.append(h("div", { class: "attachment-row" },
          h("span", {}, "attachment"),
          h("span", { class: "digest" }, part.artifact_id),
        ));
      } else {
        wrap.append(h("div", { class: "attachment-row" },
          h("span", {}, part.type),
          h("span", { class: "digest" }, part.uri),
          h("span", {}, part.mime_type),
        ));
      }
    }
    // Live partial text wins while a run streams into this message.
    const partial = app.streamText[message.id];
    if (partial) {
      const md = h("div", { class: "md" });
      renderMarkdown(md, partial);
      wrap.append(md);
    }
    return wrap;
  }

  function messageMeta(message: MessageView): HTMLElement {
    const meta = h("div", { class: "turn-meta" },
      h("span", { class: "role" }, message.role),
      h("span", {}, datetimeText(message.created_at)),
    );
    const run = message.run_id ? app.runs[message.run_id] : undefined;
    if (message.run_id) {
      const status = run?.status ?? message.status;
      meta.append(pill(status, undefined));
      meta.append(h("span", { class: "engine", title: message.run_id }, `run ${message.run_id.slice(0, 8)}`));
    }
    const snapshot = message.snapshot;
    if (snapshot) {
      meta.append(h("span", { class: "engine" }, `${snapshot.route.connection_id}/${snapshot.route.model_id}`));
    }
    return meta;
  }

  function branchLabel(branch: BranchView, history: ConversationHistory): string {
    if (history.branches.length <= 1) return "";
    const index = history.branches.findIndex((b) => b.id === branch.id);
    const op = branch.provenance.operation;
    return op === "root" ? `branch ${index + 1}` : `branch ${index + 1} · ${op}`;
  }

  function renderBranch(history: ConversationHistory, branch: BranchView): HTMLElement {
    const label = branchLabel(branch, history);
    const box = h("div", { class: "branch" });
    if (label) {
      box.append(h("div", { class: "note info" }, label));
    }
    for (const message of branch.messages) {
      const turn = h("div", { class: `turn ${message.role === "user" ? "user" : "assistant"}` },
        messageMeta(message),
        messageBody(message),
      );
      // Edit: opens a branch from this user message (P2 `conversation_edit`).
      if (message.role === "user") {
        turn.append(h("div", { style: "margin-top:4px;" },
          h("button", { class: "subtle icon", type: "button", title: "Edit this message (creates a branch)" }, "Edit"),
        ));
        turn.querySelectorAll("button").forEach((button) => {
          button.addEventListener("click", () => void editMessage(message.id));
        });
      }
      // Regenerate: re-queues the assistant reply on a new branch.
      if (message.role === "assistant" && message.run_id && message.snapshot) {
        const regen = h("button", { class: "subtle icon", type: "button" }, "Regenerate");
        regen.addEventListener("click", () => void regenerate(message.id, message.snapshot.route));
        turn.append(regen);
      }
      // Citations: exact excerpts from the snapshot context, opened via P3.
      const opener = opener();
      if (opener && message.snapshot) {
        const citations = renderContextCitations(opener, message.snapshot.context.excerpts);
        if (citations) turn.append(citations);
      }
      box.append(turn);
    }
    return box;
  }

  function activeRunIdFor(): string | undefined {
    const branch = activeBranch();
    if (!branch) return undefined;
    for (const message of branch.branch.messages) {
      if (!message.run_id) continue;
      const run = app.runs[message.run_id];
      if (run && !["succeeded", "failed", "cancelled"].includes(run.status)) return run.run_id;
    }
    return undefined;
  }

  function renderStream(): void {
    const history = app.history;
    if (!history) {
      if (app.historyError) {
        body.replaceChildren(h("div", { class: "narrow" }, note(app.historyError, "err")));
        composerSlot.replaceChildren();
        composer = undefined;
        attachments = undefined;
        return;
      }
      renderWelcome();
      composerSlot.replaceChildren();
      composer = undefined;
      attachments = undefined;
      return;
    }
    const stream = h("div", { class: "narrow conv-stream" });
    if (history.branches.length === 0) {
      stream.append(note("This conversation has no messages yet.", "info"));
    }
    for (const branch of history.branches) stream.append(renderBranch(history, branch));
    body.replaceChildren(stream);
    body.scrollTop = body.scrollHeight;

    const branch = activeBranch();
    const project = selectedProject();
    if (!branch || !project) return;

    if (!attachments) {
      attachments = mountAttachments(getClient(), project.id);
      attachments.onChange(() => {
        composer?.setAttachments(attachments?.rows() ?? []);
      });
    }

    const activeRunId = activeRunIdFor();
    if (composer && composerConversationId === history.id && composerBranchId === branch.branch.id) {
      // In-place update: the draft, focus and pending request identity survive.
      composer.setRoutes(app.routes, app.routesBlocked, app.harnessKinds);
      composer.setAttachments(attachments.rows());
      composer.setActiveRun(activeRunId);
      return;
    }

    composer?.root.remove();
    composerConversationId = history.id;
    composerBranchId = branch.branch.id;
    const next = mountComposer({
      client: getClient(),
      conversationId: history.id,
      branchId: branch.branch.id,
      routes: app.routes,
      blocked: app.routesBlocked,
      harnesses: app.harnessKinds,
      attachments: attachments.rows(),
      context: undefined,
      activeRunId,
      onSubmit: async (draft: ComposerDraft) => {
        const queued = await getClient().command<QueuedTurn>({
          type: "conversation_queue_turn",
          conversation_id: draft.conversation_id,
          branch_id: draft.branch_id,
          parts: draft.parts,
          pin: draft.pin,
          ...(draft.context ? { context: draft.context } : {}),
        }, draft.requestId);
        app.runs[queued.run_id] = {
          run_id: queued.run_id,
          conversation_id: history.id,
          branch_id: queued.branch_id,
          status: "running",
        };
        await refreshConversation();
      },
      onStop: async (runId: string) => {
        await getClient().command<unknown>({ type: "run_interrupt", run_id: runId });
        await refreshConversation();
      },
    });
    composer = next;
    composerSlot.replaceChildren(next.root);
  }

  function render(): void {
    renderHeader();
    renderStream();
  }

  // ── P2 mutations ────────────────────────────────────────────────────

  async function rename(id: string): void {
    const history = app.history;
    if (!history) return;
    const title = window.prompt("Rename conversation:", history.title);
    if (title === undefined) return;
    try {
      await getClient().command<{ done: true }>({ type: "conversation_rename", conversation_id: id, title });
      await refreshConversation();
      await refreshRecent();
    } catch (err) {
      recordError("conversation_rename", err);
      renderErrorNote(err);
    }
  }

  async function archive(id: string): void {
    try {
      await getClient().command<{ done: true }>({ type: "conversation_archive", conversation_id: id });
      app.activeConversationId = undefined;
      app.history = undefined;
      await refreshRecent();
      render();
    } catch (err) {
      recordError("conversation_archive", err);
      renderErrorNote(err);
    }
  }

  async function del(id: string): void {
    const history = app.history;
    if (!history) return;
    if (!window.confirm("Delete this conversation? Execution receipts are retained for audit; history and exports are hidden.")) return;
    const run_ids: string[] = [];
    for (const branch of history.branches) {
      for (const message of branch.messages) {
        if (message.run_id && !run_ids.includes(message.run_id)) run_ids.push(message.run_id);
      }
    }
    const resolution: RunResolution = { run_ids };
    try {
      await getClient().command<{ done: true }>({ type: "conversation_delete", conversation_id: id, resolution });
      app.activeConversationId = undefined;
      app.history = undefined;
      await refreshRecent();
      render();
    } catch (err) {
      recordError("conversation_delete", err);
      renderErrorNote(err);
    }
  }

  async function exportMarkdown(id: string): Promise<void> {
    try {
      const format: ExportFormat = "markdown";
      const exported = await getClient().command<{ content: string }>({ type: "conversation_export", conversation_id: id, format });
      const pre = h("pre", {}, exported.content);
      const panel = h("div", { class: "panel" }, h("h2", {}, "Export (markdown)"), pre);
      body.append(panel);
      body.scrollTop = body.scrollHeight;
    } catch (err) {
      recordError("conversation_export", err);
      renderErrorNote(err);
    }
  }

  async function editMessage(messageId: string): Promise<void> {
    const history = app.history;
    if (!history) return;
    const message = findMessage(history, messageId);
    if (!message) return;
    const current = message.parts.filter((p) => p.type === "text").map((p) => (p.type === "text" ? p.text : "")).join("\n");
    const next = window.prompt("Edit this user message (creates a branch):", current);
    if (next === undefined) return;
    try {
      const branchId = await getClient().command<string>({ type: "conversation_edit", message_id: messageId, parts: [{ type: "text", text: next }] });
      app.streamText = {};
      await refreshConversation();
      // The edit returns the new active branch; the composer re-mounts on the
      // newest branch automatically on the next redraw.
      void branchId;
    } catch (err) {
      recordError("conversation_edit", err);
      renderErrorNote(err);
    }
  }

  async function regenerate(messageId: string, pin: RoutePin): Promise<void> {
    try {
      const queued = await getClient().command<QueuedTurn>({ type: "conversation_regenerate", message_id: messageId, pin });
      app.runs[queued.run_id] = {
        run_id: queued.run_id,
        conversation_id: app.history?.id,
        branch_id: queued.branch_id,
        status: "running",
      };
      await refreshConversation();
    } catch (err) {
      recordError("conversation_regenerate", err);
      renderErrorNote(err);
    }
  }

  function findMessage(history: ConversationHistory, id: string): MessageView | undefined {
    for (const branch of history.branches) {
      const hit = branch.messages.find((m) => m.id === id);
      if (hit) return hit;
    }
    return undefined;
  }

  function renderErrorNote(err: unknown): void {
    const el = h("div", { class: "narrow" }, coreErrorNote(err));
    body.append(el);
    body.scrollTop = body.scrollHeight;
  }

  return { root, render };
}
