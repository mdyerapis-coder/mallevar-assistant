// Projection glue: turns core replies and durable events into the view model
// the webview renders. The webview never speaks the wire protocol directly —
// only typed command results and EventEnvelope JSON.
import type { DoctorReply, EventEnvelope, Project, StatusReply } from "./core-client";

export interface DoctorInfo {
  state: "ok" | "error";
  details?: DoctorReply;
  error?: string;
}

export interface EventItem {
  seq: number;
  timestamp: number;
  label: string;
}

export interface Projection {
  core: "connecting" | "live" | "error";
  status?: StatusReply;
  doctor: DoctorInfo;
  projects: Project[];
  projectsError?: string;
  events: EventItem[];
  lastSeq: number;
  conversation?: {
    id: string;
    project_id: string;
    messages: Array<{ id: string; role: string; parts: Array<{ text?: string }> }>;
  };
}

export function initialProjection(): Projection {
  return { core: "connecting", doctor: { state: "ok", details: undefined }, projects: [], events: [], lastSeq: 0 };
}

function eventLabel(e: EventEnvelope): string {
  const p = e.payload;
  switch (p.type) {
    case "state_changed":
      return `state_changed ${p.entity_id} → ${p.state}`;
    case "artifact_added":
      return `artifact_added ${p.artifact_id} ${p.digest}`;
    default:
      return p.type;
  }
}

export function applyStatus(proj: Projection, status: StatusReply): Projection {
  return { ...proj, core: "live", status };
}

export function applyDoctorOk(proj: Projection, details: DoctorReply): Projection {
  return { ...proj, doctor: { state: "ok", details } };
}

export function applyDoctorError(proj: Projection, error: string): Projection {
  return { ...proj, doctor: { state: "error", error } };
}

export function applyProjects(proj: Projection, projects: Project[]): Projection {
  return { ...proj, projects, projectsError: undefined };
}

export function applyProjectsError(proj: Projection, error: string): Projection {
  return { ...proj, projectsError: error };
}

/** Appends one durable event, keeping the newest 200 and the cursor high-water. */
export function applyEvent(proj: Projection, e: EventEnvelope): Projection {
  if (e.seq <= proj.lastSeq) return proj;
  const events = [...proj.events, { seq: e.seq, timestamp: e.timestamp, label: eventLabel(e) }].slice(-200);
  return { ...proj, events, lastSeq: e.seq };
}

export function coreState(proj: Projection, state: "connecting" | "live" | "error"): Projection {
  return { ...proj, core: state };
}

export function render(proj: Projection, root: HTMLElement, view: string): void {
  if (view === "conversation") {
    renderConversation(proj, root);
    return;
  }

  const rows: Array<[string, string]> = [];
  rows.push(["core", proj.core]);
  if (proj.status) {
    rows.push(["projects", String(proj.status.projects)]);
    rows.push(["last_seq", String(proj.status.last_seq)]);
    rows.push(["abandoned_requests", String(proj.status.abandoned_requests)]);
  }
  const d = proj.doctor;
  rows.push(["sqlite", d.state === "error" ? `error: ${d.error ?? "unknown"}` : d.details ? d.details.sqlite_integrity : "pending"]);
  rows.push(["schema", d.details ? String(d.details.schema_version) : "pending"]);
  rows.push(["journal", d.details ? d.details.journal_mode : "pending"]);

  root.replaceChildren(
    h("div", { class: "pane" },
      h("h2", {}, "Core status"),
      h("dl", {}, ...rows.map(([k, v]) => h("div", { class: "kv" }, h("dt", {}, k), h("dd", {}, v)))),
    ),
    h("div", { class: "pane" },
      h("h2", {}, `Projects (${proj.projects.length})`),
      proj.projectsError
        ? h("p", { class: "error" }, proj.projectsError)
        : h("ul", {}, ...proj.projects.map((p) => h("li", {}, `${p.name} — ${p.root}`))),
    ),
    h("div", { class: "pane events" },
      h("h2", {}, `Events (${proj.events.length})`),
      h("ol", {}, ...proj.events.slice().reverse().map((e) => h("li", {}, `#${e.seq} ${e.label}`))),
    ),
  );
}

function renderConversation(proj: Projection, root: HTMLElement): void {
  const messages = proj.conversation?.messages ?? [];
  root.replaceChildren(
    h("div", { class: "pane conversation" },
      h("h2", {}, "Conversations"),
      h("div", { class: "messages" }, ...messages.map((m) =>
        h("div", { class: `message ${m.role}` },
          h("div", { class: "role" }, m.role),
          h("div", { class: "content" }, m.parts.map((p) => p.text ?? "").join("")),
        ),
      )),
      h("div", { class: "input-row" },
        h("input", { class: "chat-input", type: "text", placeholder: "Type a message..." }),
        h("button", { class: "primary", type: "button", onclick: "window.dispatchEvent(new CustomEvent('send-message'))" }, "Send"),
      ),
    ),
  );
}

function h(tag: string, attrs: Record<string, string> = {}, ...children: Array<Node | string>): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  for (const c of children) el.append(c);
  return el;
}
