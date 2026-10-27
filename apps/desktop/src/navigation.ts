// Navigation shell for the D2/D3 desktop (plan §2). Owns the canonical view
// list + labels and the navigate callback; the left sidebar renders it. The
// webview is read-only with respect to durable state: navigation only switches
// which projected view the user observes.
import type { ViewName } from "./app";

export interface NavItem {
  name: ViewName;
  label: string;
}

export const NAV_ITEMS: NavItem[] = [
  { name: "conversation", label: "Conversations" },
  { name: "tasks", label: "Tasks" },
  { name: "knowledge", label: "Knowledge" },
  { name: "approvals", label: "Approvals" },
  { name: "settings", label: "Settings" },
  { name: "recovery", label: "Recovery" },
  { name: "status", label: "Core status" },
  { name: "events", label: "Events" },
];

let onNavigate: (view: ViewName) => void = () => {};

/** main.ts wires this so the sidebar rail can drive view switches. */
export function setNavigateHandler(handler: (view: ViewName) => void): void {
  onNavigate = handler;
}

export function navigate(view: ViewName): void {
  onNavigate(view);
}

export function renderNavigation(container: HTMLElement, current: ViewName): void {
  container.innerHTML = "";
  for (const item of NAV_ITEMS) {
    const a = document.createElement("a");
    a.href = `#${item.name}`;
    a.textContent = item.label;
    a.className = item.name === current ? "nav-item active" : "nav-item";
    a.onclick = (e) => {
      e.preventDefault();
      navigate(item.name);
    };
    container.appendChild(a);
  }
}