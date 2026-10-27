// D2/D3 application spine: one core subscription, a small set of live
// projections the views render, and the event routing that keeps them fresh.
// The webview stays read-only with respect to durable state: every mutation
// goes through a typed core command, and the projections only reflect what
// the core answered or streamed.
import type { ConnectionDescriptor, ConversationHistory, HarnessDescriptor, ProjectPolicy, QualificationRecord } from "@ironbark/contracts";
import type { CoreClient, CoreEventPayload, EventEnvelope, Project } from "./core-client";
import { CoreError } from "./core-client";
import { h } from "./components/dom";
import type { RouteOption } from "./components/composer";

import type { ViewName } from "./navigation";

export type { ViewName };
export interface RunState {
  run_id: string;
  conversation_id?: string;
  branch_id?: string;
  message_id?: string;
  status: string;
}

export interface AppError {
  scope: string;
  code: string;
  message: string;
  at: number;
}

export interface AppState {
  projects: Project[];
  core: "connecting" | "live" | "error";
  statusError?: string;
  routes: RouteOption[];
  routesBlocked: Record<string, string>;
  harnessKinds: string[];
  projectsError?: string;
  selectedProjectId?: string;
  recent: Array<{ id: string; title: string; archived: boolean; created_at: number }>;
  activeConversationId?: string;
  history?: ConversationHistory;
  historyError?: string;
  /** Live partial text for assistant messages, keyed by message id. */
  streamText: Record<string, string>;
  runs: Record<string, RunState>;
  lastSeq: number;
  errors: AppError[];
  railCollapsed: boolean;
}

export const app: AppState = {
  projects: [],
  core: "connecting",
  routes: [],
  routesBlocked: {},
  harnessKinds: [],
  recent: [],
  streamText: {},
  runs: {},
  lastSeq: 0,
  errors: [],
  railCollapsed: false,
};

let client: CoreClient | null = null;
let redraw: () => void = () => {};
let refreshTimer: number | undefined;

export function setClient(value: CoreClient): void {
  client = value;
}

export function getClient(): CoreClient {
  if (!client) throw new Error("core client not initialised");
  return client;
}

/** Stable core-error extraction: the code is the UI's real capability signal. */
export function recordError(scope: string, err: unknown): { code: string; message: string } {
  const code = err instanceof CoreError ? err.code : "transport";
  const message = err instanceof Error ? err.message : String(err);
  app.errors.unshift({ scope, code, message, at: Date.now() });
  app.errors = app.errors.slice(0, 50);
  return { code, message };
}

export function selectedProject(): Project | undefined {
  return app.projects.find((p) => p.id === app.selectedProjectId) ?? app.projects[0];
}

function hasActiveRun(conversationId?: string): boolean {
  return Object.values(app.runs).some(
    (run) => run.conversation_id === conversationId && !["succeeded", "failed", "cancelled"].includes(run.status),
  );
}

function scheduleHistoryRefresh(): void {
  if (refreshTimer !== undefined) return;
  refreshTimer = window.setTimeout(() => {
    refreshTimer = undefined;
    void refreshConversation();
  }, 800);
}

/** Refetches the active conversation's history from the core. */
export async function refreshConversation(): Promise<void> {
  const id = app.activeConversationId;
  if (!id) return;
  try {
    const history = await getClient().command<ConversationHistory>({ type: "conversation_history", conversation_id: id });
    app.history = history;
    app.historyError = undefined;
    indexRuns(history);
  } catch (err) {
    app.historyError = recordError("conversation_history", err).message;
  }
  redraw();
}

export async function refreshRecent(): Promise<void> {
  const project = selectedProject();
  if (!project) return;
  try {
    app.recent = await getClient().command<Array<AppState["recent"][number]>>({
      type: "conversation_search",
      project_id: project.id,
      query: "",
      include_archived: false,
      limit: 50,
    });
  } catch (err) {
    recordError("conversation_search", err);
  }
  redraw();
}

/** Loads the composer's capability surface from the core: provider
 * connections (model/provider route options + blocked reasons) and the
 * available harness kinds. Blocked reasons are derived from the core's own
 * descriptors — an unavailable/unqualified connection or a missing credential
 * is a real reason, never a fake disabled control. */
export async function refreshCapabilities(): Promise<void> {
  try {
    const connections = await getClient().command<ConnectionDescriptor[]>({ type: "connections_list" });
    const routes: RouteOption[] = [];
    const blocked: Record<string, string> = {};
    for (const connection of connections) {
      let reason: string | undefined;
      if (connection.availability === "unavailable") reason = `provider unreachable: ${connection.endpoint}`;
      else if (!connection.auth_available) reason = "credentials not configured (set them in Settings → Connections)";
      for (const model of connection.models) {
        const id = `${connection.id}/${model.id}`;
        routes.push({ connection, model });
        if (reason) blocked[id] = reason;
      }
    }
    app.routes = routes;
    app.routesBlocked = blocked;
  } catch (err) {
    recordError("connections_list", err);
    app.routes = [];
    app.routesBlocked = {};
  }
  try {
    const descriptors = await getClient().command<HarnessDescriptor[]>({ type: "harness_descriptors" });
    app.harnessKinds = descriptors.map((d) => d.harness);
  } catch (err) {
    recordError("harness_descriptors", err);
    app.harnessKinds = [];
  }
  redraw();
}

/** Extracts run/message association and status from a durable history. */
export function indexRuns(history: ConversationHistory): void {
  for (const branch of history.branches) {
    for (const message of branch.messages) {
      if (!message.run_id) continue;
      const run = app.runs[message.run_id] ?? {
        run_id: message.run_id,
        conversation_id: history.id,
        branch_id: branch.id,
      };
      run.conversation_id = history.id;
      run.branch_id = branch.id;
      run.message_id = message.id;
      if (!["succeeded", "failed", "cancelled"].includes(message.status)) {
        run.status = mapMessageStatusToRun(message.status);
      }
      app.runs[message.run_id] = run;
    }
  }
}

function mapMessageStatusToRun(status: string): string {
  switch (status) {
    case "queued":
    case "streaming":
      return "running";
    case "failed":
      return "failed";
    case "interrupted":
      return "cancelled";
    case "complete":
      return "succeeded";
    default:
      return status;
  }
}

interface EnvelopeFields {
  type: string;
  message_id?: string;
  delta?: string;
  text?: string;
  entity_id?: string;
  state?: string;
  status?: string;
}

function fieldsOf(payload: CoreEventPayload): EnvelopeFields {
  const record = payload as unknown as Record<string, unknown>;
  return {
    type: record.type === undefined ? "" : String(record.type),
    message_id: typeof record.message_id === "string" ? record.message_id : undefined,
    delta: typeof record.delta === "string" ? record.delta : undefined,
    text: typeof record.text === "string" ? record.text : undefined,
    entity_id: typeof record.entity_id === "string" ? record.entity_id : undefined,
    state: typeof record.state === "string" ? record.state : undefined,
    status: typeof record.status === "string" ? record.status : undefined,
  };
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Routes one durable envelope into the projections and redraws. */
export function onEvent(envelope: EventEnvelope): void {
  if (envelope.seq <= app.lastSeq) return;
  app.lastSeq = Math.max(app.lastSeq, envelope.seq);
  const p = fieldsOf(envelope.payload);

  if (p.message_id && (p.type === "text_delta" || p.type === "text_replace")) {
    const current = app.streamText[p.message_id] ?? "";
    app.streamText[p.message_id] = p.type === "text_delta" ? current + (p.delta ?? "") : (p.text ?? "");
  }

  if (p.type === "state_changed" && p.entity_id && p.state && UUID.test(p.entity_id)) {
    const run = app.runs[p.entity_id] ?? { run_id: p.entity_id, status: p.state };
    run.status = p.state;
    app.runs[p.entity_id] = run;
    if (run.conversation_id === app.activeConversationId) scheduleHistoryRefresh();
  }

  if (envelope.run_id && p.type === "turn_finished") {
    const run = app.runs[envelope.run_id];
    if (run && p.status) run.status = p.status;
    if (run && run.conversation_id === app.activeConversationId) scheduleHistoryRefresh();
  }

  if (p.type === "artifact_added") scheduleHistoryRefresh();
  if (hasActiveRun(app.activeConversationId)) scheduleHistoryRefresh();

  redraw();
}

export function setRedraw(fn: () => void): void {
  redraw = fn;
}

export function renderConnectionBanner(): HTMLElement {
  if (app.core === "live") return h("div", { class: "banner" }, "core live");
  if (app.core === "error") return h("div", { class: "banner error" }, app.statusError ?? "core error");
  return h("div", { class: "banner wait" }, "connecting to core…");
}

export function coreErrorNote(err: unknown): HTMLElement {
  const code = err instanceof CoreError ? err.code : "transport";
  const message = err instanceof Error ? err.message : String(err);
  const hint =
    code === "invalid_command"
      ? " The core has not wired this command yet; this control is shown in its real unsupported state."
      : "";
  return h("div", { class: "reason-note error" }, `core ${code}: ${message}.`, hint);
}
