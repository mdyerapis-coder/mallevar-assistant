// D2 left sidebar: brand, primary view navigation, project selector, recent
// conversations (via the wired P2 `conversation_search`), approved artwork on
// the quiet surface, and the core connection state. The sidebar never holds
// working text over artwork: the image is a bounded strip with its own
// background.
import lockArt from "../assets/ironbark-lock.jpg";
import type { Project } from "../core-client";
import { app, type ViewName } from "../app";
import { dateTimeText, h } from "../components/dom";

export interface SidebarConfig {
  view: ViewName;
  projects: Project[];
  onNavigate: (view: ViewName) => void;
  onSelectProject: (id: string) => void;
  onSelectConversation: (id: string) => void;
  onNewConversation: () => void;
}

const NAV: Array<{ name: ViewName; label: string }> = [
  { name: "conversation", label: "Conversations" },
  { name: "tasks", label: "Tasks" },
  { name: "knowledge", label: "Knowledge" },
  { name: "approvals", label: "Approvals" },
  { name: "settings", label: "Settings" },
  { name: "recovery", label: "Recovery" },
  { name: "status", label: "Core status" },
  { name: "events", label: "Events" },
];

export function renderSidebar(root: HTMLElement, config: SidebarConfig): void {
  const brand = h("div", { class: "sidebar-brand" },
    h("span", { class: "brand-mark", "aria-hidden": "true" }),
    "Ironbark Assistant",
  );

  const nav = h("nav", { class: "sidebar-nav", "aria-label": "Primary" });
  for (const item of NAV) {
    const link = h("a", {
      href: `#${item.name}`,
      class: item.name === config.view ? "active" : "",
      "aria-current": item.name === config.view ? "page" : undefined,
    }, item.label);
    link.addEventListener("click", (ev) => {
      ev.preventDefault();
      config.onNavigate(item.name);
    });
    nav.append(link);
  }

  // Project selector: the active project scopes conversations and knowledge.
  const projectSelect = h("select", { "aria-label": "Project" });
  projectSelect.append(h("option", { value: "" }, config.projects.length === 0 ? "No projects" : "Select project…"));
  for (const project of config.projects) {
    const option = h("option", { value: project.id }, `${project.name} (${project.root})`);
    if (project.id === app.selectedProjectId) option.selected = true;
    projectSelect.append(option);
  }
  projectSelect.addEventListener("change", () => {
    if (projectSelect.value !== "") config.onSelectProject(projectSelect.value);
  });
  const projectBox = h("div", { class: "sidebar-project", style: "padding:0 12px 4px;" }, projectSelect);

  const section = h("div", { class: "sidebar-section-title" }, "Recent conversations");
  const list = h("div", { class: "sidebar-conversations" });
  if (app.recent.length === 0) {
    list.append(h("div", { class: "note", style: "padding:4px 10px;" },
      config.projects.length === 0 ? "Create a project to start a conversation." : "No conversations yet.",
    ));
  }
  for (const item of app.recent) {
    const row = h("button", {
      class: `conversation-row ${item.id === app.activeConversationId ? "active" : ""}`,
      type: "button",
      "aria-disabled": item.archived ? "true" : undefined,
    },
      h("span", {}, item.title || "Untitled"),
      h("span", { class: "row-sub" }, dateTimeText(item.created_at)),
    );
    row.addEventListener("click", () => config.onSelectConversation(item.id));
    list.append(row);
  }

  const newButton = h("button", { class: "subtle", type: "button", style: "margin:6px 12px;" }, "+ New conversation");
  newButton.addEventListener("click", () => config.onNewConversation());

  const stateClass = app.core === "live" ? "ok" : app.core === "error" ? "error" : "";
  const stateLabel =
    app.core === "live" ? "core live" : app.core === "error" ? `core error: ${app.statusError ?? "unknown"}` : "connecting…";
  const state = h("div", { class: `sidebar-state ${stateClass}` }, stateLabel);

  const bottom = h("div", { class: "sidebar-bottom" },
    state,
    h("img", {
      class: "sidebar-art",
      src: lockArt,
      alt: "Ironbark lock — approved artwork",
      loading: "lazy",
    }),
  );

  root.replaceChildren(brand, nav, projectBox, section, list, newButton, bottom);
}
