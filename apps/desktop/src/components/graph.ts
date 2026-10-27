// Lightweight task-graph rendering for the Tasks view. Nodes are jobs/runs
// with role, harness (native identity), dependency and status. The graph is a
// vertical indented tree — no canvas — so keyboard focus and 200% text scale
// stay intact. Unknown/missing native children are shown truthfully.
import { h, jsonSummary, pill } from "./dom";

export interface GraphNodeInput {
  id: string;
  label: string;
  depth: number;
  role?: string;
  harness?: string;
  nativeId?: string;
  status: string;
  dependsOn?: string[];
  detail?: string;
}

/** Renders the node list. Dependency edges are shown as text (D: x, y) to
 * keep the surface readable and keyboard-navigable at any scale. */
export function renderGraph(root: HTMLElement, nodes: GraphNodeInput[]): void {
  root.replaceChildren();
  if (nodes.length === 0) {
    root.append(h("p", { class: "note info" }, "No task nodes observed. Runs appear here with their roles, harness identities and dependencies."));
    return;
  }
  const graph = h("div", { class: "graph" });
  const labelById = new Map(nodes.map((n) => [n.id, n.label]));
  for (const node of nodes) {
    const el = h("div", { class: `graph-node depth-${Math.min(node.depth, 2)} ${node.status}`, tabindex: "0" },
      h("div", { class: "node-title" }, node.label),
      h("div", { class: "node-side" }, pill(node.status)),
    );
    const metaBits: string[] = [];
    if (node.role) metaBits.push(`role: ${node.role}`);
    if (node.harness) metaBits.push(`harness: ${node.harness}`);
    if (node.nativeId) metaBits.push(`native: ${node.nativeId} (opaque)`);
    if (node.dependsOn && node.dependsOn.length > 0) {
      metaBits.push(`depends on: ${node.dependsOn.map((d) => labelById.get(d) ?? d).join(", ")}`);
    }
    metaBits.push(node.id);
    el.append(h("div", { class: "node-meta" }, metaBits.join(" · ")));
    if (node.detail) el.append(h("div", { class: "node-meta" }, node.detail));
    graph.append(el);
  }
  root.append(graph);
}

/** Compact dependency summary for a node. */
export function dependencySummary(dependsOn?: string[], labelById?: Map<string, string>): string {
  if (!dependsOn || dependsOn.length === 0) return "no dependencies";
  return dependsOn.map((d) => (labelById?.get(d) ?? d)).join(", ");
}

/** JSON detail kept short so the graph stays readable. */
export function detail(value: unknown): string {
  return jsonSummary(value, 200);
}
