// D3 Settings surface: provider connections (create/probe/import-preview),
// project policy (route budgets), harness versions/capabilities, WebGPT
// activation, and JEV domain reports. Raw secrets never enter JS — the
// native `open_credential_dialog` returns only the caller's AuthRef.
import type { ConnectionConfig, ConnectionDescriptor, HarnessDescriptor, ProjectPolicy, QualificationRecord } from "@ironbark/contracts";
import { invoke } from "@tauri-apps/api/core";
import { app, coreErrorNote, getClient, recordError } from "../app";
import { datetimeText, digestView, h, jsonSummary, kvRow, note, pill } from "../components/dom";

const PROTOCOLS = ["openai_completions", "openai_responses", "anthropic", "gemini", "ollama", "llama_cpp", "custom"] as const;
function isProtocol(value: string): value is (typeof PROTOCOLS)[number] {
  return PROTOCOLS.includes(value as (typeof PROTOCOLS)[number]);
}
const HARNESSES = ["omp", "codex", "opencode", "claude", "hermes", "alphacode"] as const;
function isHarness(value: string): value is (typeof HARNESSES)[number] {
  return HARNESSES.includes(value as (typeof HARNESSES)[number]);
}

export interface SettingsView {
  root: HTMLElement;
  render(): void;
}

export function mountSettingsView(): SettingsView {
  const root = h("div", { class: "surface settings-surface" });
  const header = h("div", { class: "surface-header" });
  const body = h("div", { class: "surface-body" });
  root.append(header, body);

  let connections: ConnectionDescriptor[] = [];
  let harnesses: HarnessDescriptor[] = [];
  let policy: ProjectPolicy | undefined;
  let jevRecords: QualificationRecord[] | undefined;
  function render(): void {
    renderHeader();
    renderBody();
  }

  function renderHeader(): void {
    header.replaceChildren(
      h("div", { class: "title" }, "Settings"),
      h("div", { class: "subtitle" }, "Connections, policy, harnesses and domain reports"),
    );
  }

  async function renderBody(): Promise<void> {
    body.replaceChildren();
    await Promise.all([refreshConnections(), refreshHarnesses(), refreshPolicy(), refreshJEV()]);
    renderConnections();
    renderPolicy();
    renderHarnesses();
    renderJEV();
    renderWebGPT();
  }

  function renderConnections(): void {
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, "Provider connections"));
    if (connections.length === 0) {
      panel.append(note("No connections registered.", "info"));
    }
    for (const conn of connections) {
      const card = h("div", { class: "connection-card" },
        h("div", { class: "conn-head" },
          h("span", {}, conn.id),
          pill(conn.availability, undefined),
          h("span", { class: "muted" }, conn.protocol),
          h("span", { class: "muted" }, conn.endpoint),
        ),
        h("div", { class: "conn-meta" },
          `models: ${conn.models.map((m) => m.id).join(", ")}`,
          conn.auth_available ? " · credentials available" : " · credentials missing",
        ),
      );
      const actions = h("div", { style: "margin-top:6px;display:flex;gap:6px;" });
      const probeBtn = h("button", { class: "subtle", type: "button" }, "Probe");
      probeBtn.addEventListener("click", async () => {
        try {
          const result = await getClient().command<ConnectionDescriptor>({ type: "connection_probe", connection_id: conn.id });
          actions.append(note(`Probe: ${result.availability} (${result.capabilities.map((c) => c.name).join(", ")})`, result.availability === "qualified" ? "" : "warn"));
        } catch (err) {
          recordError("connection_probe", err);
          actions.append(coreErrorNote(err));
        }
      });
      const delBtn = h("button", { class: "danger", type: "button" }, "Delete");
      delBtn.addEventListener("click", async () => {
        if (!window.confirm(`Delete connection ${conn.id}?`)) return;
        try {
          await getClient().command<{ done: true }>({ type: "connection_delete", connection_id: conn.id });
          await refreshConnections();
        } catch (err) {
          recordError("connection_delete", err);
          actions.append(coreErrorNote(err));
        }
      });
      actions.append(probeBtn, delBtn);
      card.append(actions);
      panel.append(card);
    }
    // Register form
    const form = h("div", { class: "connection-form", style: "margin-top:12px;padding:8px;border-top:1px solid var(--border)" });
    form.append(h("h3", { style: "margin:0 0 8px;" }, "Register new connection"));
    const idInput = h("input", { type: "text", placeholder: "Connection ID", style: "width:200px;padding:4px 8px;" });
    const protoSel = h("select", { style: "width:200px;padding:4px 8px;" },
      h("option", { value: "" }, "Protocol"),
      h("option", { value: "openai_completions" }, "openai_completions"),
      h("option", { value: "openai_responses" }, "openai_responses"),
      h("option", { value: "anthropic" }, "anthropic"),
      h("option", { value: "gemini" }, "gemini"),
      h("option", { value: "ollama" }, "ollama"),
      h("option", { value: "llama_cpp" }, "llama_cpp"),
      h("option", { value: "custom" }, "custom"),
    );
    const endpointInput = h("input", { type: "text", placeholder: "Endpoint URL", style: "width:300px;padding:4px 8px;" });
    const modelsInput = h("input", { type: "text", placeholder: "Model IDs (comma-separated)", style: "width:300px;padding:4px 8px;" });
    const authType = h("select", { style: "width:200px;padding:4px 8px;" },
      h("option", { value: "secret_service" }, "Secret Service (native dialog)"),
      h("option", { value: "bitwarden" }, "Bitwarden (item_id + field)"),
      h("option", { value: "native_session" }, "Native session (harness profile)"),
    );
    const authDetails = h("div", { style: "margin-top:6px;display:flex;flex-wrap:wrap;gap:6px;" });
    const registerBtn = h("button", { class: "primary", type: "button" }, "Register");
    registerBtn.addEventListener("click", async () => {
      const models = modelsInput.value.split(",").map((m) => m.trim()).filter((m) => m);
      if (!idInput.value || !protoSel.value || !endpointInput.value || models.length === 0) return;
      const proto = protoSel.value;
      if (!isProtocol(proto)) {
        form.append(note("Invalid protocol.", "err"));
        return;
      }
      let authRef: ConnectionConfig["auth_ref"];
      if (authType.value === "secret_service") {
        try {
          const ref = await invoke<{ service: string; account: string }>("open_credential_dialog", { reference: { type: "secret_service", service: idInput.value, account: "default" } });
          authRef = { type: "secret_service", service: ref.service, account: ref.account };
        } catch (err) {
          form.append(coreErrorNote(err));
          return;
        }
      } else if (authType.value === "bitwarden") {
        const itemId = window.prompt("Bitwarden item ID:");
        const field = window.prompt("Field name:");
        if (!itemId || !field) return;
        authRef = { type: "bitwarden", item_id: itemId, field };
      } else {
        const harness = window.prompt("Harness kind (omp|codex|opencode|claude|hermes|alphacode):");
        const profile = window.prompt("Profile name:");
        if (!harness || !profile) return;
        if (!isHarness(harness)) {
          form.append(note("Invalid harness kind.", "err"));
          return;
        }
        authRef = { type: "native_session", harness, profile };
      }
      try {
        await getClient().command<ConnectionDescriptor>({
          type: "connection_register",
          config: { id: idInput.value, protocol: proto, endpoint: endpointInput.value, models: models.map((id) => ({ id, context_limit: 0, output_limit: 0, capabilities: [] })), auth_ref: authRef, allowed_projects: [] },
        });
        idInput.value = "";
        endpointInput.value = "";
        modelsInput.value = "";
        await refreshConnections();
      } catch (err) {
        recordError("connection_register", err);
        form.append(coreErrorNote(err));
      }
    });
    form.append(idInput, protoSel, endpointInput, modelsInput, authType, authDetails, registerBtn);
    panel.append(form);
    body.append(panel);
  }

  function renderPolicy(): void {
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, "Project policy (route budgets)"));
    if (!policy) {
      panel.append(note("Project policy not available. Select a project and ensure the core wires the policy_get command.", "info"));
      return;
    }
    panel.append(h("div", {},
      kvRow("version", String(policy.version)),
      kvRow("allowed connections", policy.allowed_connection_ids.join(", ") || "all"),
      kvRow("allowed tools", policy.allowed_tools.join(", ") || "all"),
      kvRow("read roots", policy.read_roots.join(", ") || "all"),
      kvRow("write roots", policy.write_roots.join(", ") || "all"),
      kvRow("network destinations", policy.network_destinations.join(", ") || "all"),
      kvRow("local only", policy.local_only ? "yes" : "no"),
      kvRow("max workers", String(policy.max_workers)),
      kvRow("max requests", String(policy.max_requests)),
      kvRow("max cost (µUSD)", policy.max_cost_microusd ? String(policy.max_cost_microusd) : "unlimited"),
      kvRow("spend policy", policy.spend_policy),
      kvRow("allow unknown cost", policy.allow_unknown_cost ? "yes" : "no"),
    ));
    // Edit form
    const form = h("div", { style: "margin-top:12px;padding:8px;border-top:1px solid var(--border)" });
    form.append(h("h3", { style: "margin:0 0 8px;" }, "Update policy"));
    const maxWorkers = h("input", { type: "number", value: String(policy.max_workers), style: "width:80px;padding:4px 8px;" });
    const maxRequests = h("input", { type: "number", value: String(policy.max_requests), style: "width:80px;padding:4px 8px;" });
    const maxCost = h("input", { type: "number", value: String(policy.max_cost_microusd ?? ""), placeholder: "max cost µUSD", style: "width:140px;padding:4px 8px;" });
    const spendSel = h("select", { style: "padding:4px 8px;" },
      h("option", { value: "free_only" }, "free_only"),
      h("option", { value: "paid_allowed" }, "paid_allowed"),
    );
    spendSel.value = policy.spend_policy;
    const allowUnknown = h("input", { type: "checkbox", checked: policy.allow_unknown_cost });
    const localOnly = h("input", { type: "checkbox", checked: policy.local_only });
    const connIds = h("input", { type: "text", value: policy.allowed_connection_ids.join(","), placeholder: "allowed connection IDs (comma-separated)", style: "width:300px;padding:4px 8px;" });
    const toolIds = h("input", { type: "text", value: policy.allowed_tools.join(","), placeholder: "allowed tools (comma-separated)", style: "width:300px;padding:4px 8px;" });
    const readRoots = h("input", { type: "text", value: policy.read_roots.join(","), placeholder: "read roots (comma-separated)", style: "width:300px;padding:4px 8px;" });
    const writeRoots = h("input", { type: "text", value: policy.write_roots.join(","), placeholder: "write roots (comma-separated)", style: "width:300px;padding:4px 8px;" });
    const netDests = h("input", { type: "text", value: policy.network_destinations.join(","), placeholder: "network destinations (comma-separated)", style: "width:300px;padding:4px 8px;" });
    const submit = h("button", { class: "primary", type: "button" }, "Apply");
    submit.addEventListener("click", async () => {
      const updated: ProjectPolicy = {
        version: policy.version + 1,
        project_id: policy.project_id,
        allowed_connection_ids: connIds.value.split(",").map((s) => s.trim()).filter((s) => s),
        allowed_tools: toolIds.value.split(",").map((s) => s.trim()).filter((s) => s),
        read_roots: readRoots.value.split(",").map((s) => s.trim()).filter((s) => s),
        write_roots: writeRoots.value.split(",").map((s) => s.trim()).filter((s) => s),
        network_destinations: netDests.value.split(",").map((s) => s.trim()).filter((s) => s),
        local_only: localOnly.checked,
        max_workers: Number(maxWorkers.value),
        max_requests: Number(maxRequests.value),
        max_cost_microusd: maxCost.value ? Number(maxCost.value) : undefined,
        spend_policy: policy.spend_policy,
        allow_unknown_cost: allowUnknown.checked,
      };
      try {
        await getClient().command<ProjectPolicy>({ type: "policy_update", policy: updated });
        await refreshPolicy();
      } catch (err) {
        recordError("policy_update", err);
        panel.append(coreErrorNote(err));
      }
    });
    form.append(
      h("div", { style: "display:flex;gap:8px;flex-wrap:wrap;margin-bottom:8px;" }, h("label", {}, "max workers", maxWorkers), h("label", {}, "max requests", maxRequests), h("label", {}, "max cost", maxCost)),
      h("div", { style: "display:flex;gap:8px;flex-wrap:wrap;margin-bottom:8px;" }, h("label", {}, "spend policy", spendSel), h("label", {}, "allow unknown cost", allowUnknown), h("label", {}, "local only", localOnly)),
      h("div", { style: "display:flex;gap:8px;flex-wrap:wrap;margin-bottom:8px;" }, connIds, toolIds),
      h("div", { style: "display:flex;gap:8px;flex-wrap:wrap;margin-bottom:8px;" }, readRoots, writeRoots),
      h("div", { style: "display:flex;gap:8px;flex-wrap:wrap;margin-bottom:8px;" }, netDests),
      submit,
    );
    panel.append(form);
    body.append(panel);
  }

  function renderHarnesses(): void {
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, "Harness versions and capabilities"));
    if (harnesses.length === 0) {
      panel.append(note("No harness descriptors reported.", "info"));
    }
    for (const h of harnesses) {
      const card = h("div", { class: "harness-card" },
        h("div", { class: "harness-head" },
          h("span", {}, h.harness),
          h("span", { class: "muted" }, `v${h.version} · ${h.transport}`),
          pill(h.approval, "approval"),
          pill(h.cancel, "cancel"),
          pill(h.resume, "resume"),
          pill(h.child_observability, "child obs"),
        ),
        h("div", { class: "harness-caps" }),
      );
      const capsSlot = card.querySelector(".harness-caps") as HTMLElement;
      for (const cap of h.capabilities) {
        capsSlot.append(h("div", { class: "kv" },
          h("span", { class: "kv-key" }, cap.name),
          pill(cap.state, cap.implementation_version),
          h("span", { class: "kv-value" }, cap.evidence_digest ? `evidence ${digestView(cap.evidence_digest)}` : ""),
        ));
      }
      panel.append(card);
    }
    body.append(panel);
  }

  function renderJEV(): void {
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, "JEV domain reports (qualification records)"));
    const records = jevRecords;
    if (records === undefined) {
      panel.append(note("JEV domain reports load when the core wires `judgment_domains`. If that dispatch arm is absent, this surface shows the real unsupported state.", "info"));
    } else if (records.length === 0) {
      panel.append(note("No qualification records reported for this project.", "info"));
    } else {
      for (const record of records) {
        panel.append(h("div", { class: "kv" },
          h("span", { class: "kv-key" }, `${record.domain} / ${record.model_identity}`),
          pill(record.state === "enabled" ? "qualified" : "blocked", record.state),
          h("span", { class: "kv-value" }, `upper error bound ≤ ${record.upper_error_bound}, acceptance ${Math.round(record.acceptance_coverage * 100)}%, ${datetimeText(record.evaluated_at)}`),
        ));
      }
    }
    body.append(panel);
  }

  function renderWebGPT(): void {
    const panel = h("div", { class: "panel" });
    panel.append(h("h2", {}, "WebGPT activation"));
    const codex = harnesses.find((h) => h.harness === "codex");
    if (codex) {
      panel.append(h("div", {},
        kvRow("harness", codex.harness),
        kvRow("version", codex.version),
        kvRow("transport", codex.transport),
        kvRow("approval", codex.approval),
        kvRow("cancel", codex.cancel),
      ));
      panel.append(note("WebGPT is delivered through the Codex harness; activation state is observed from its capability reports.", "info"));
    } else {
      panel.append(note("Codex harness not reported; WebGPT activation state unavailable.", "warn"));
    }
    body.append(panel);
  }

  async function refreshConnections(): Promise<void> {
    try {
      connections = await getClient().command<ConnectionDescriptor[]>({ type: "connections_list" });
    } catch (err) {
      recordError("connections_list", err);
      connections = [];
    }
  }

  async function refreshHarnesses(): Promise<void> {
    try {
      harnesses = await getClient().command<HarnessDescriptor[]>({ type: "harness_descriptors" });
    } catch (err) {
      recordError("harness_descriptors", err);
      harnesses = [];
    }
  }

  async function refreshJEV(): Promise<void> {
    try {
      jevRecords = await getClient().command<QualificationRecord[]>({ type: "judgment_domains" });
    } catch (err) {
      recordError("judgment_domains", err);
      jevRecords = undefined;
    }
  }

  return { root, render };
}

function selectedProject() {
  return app.projects.find((p) => p.id === app.selectedProjectId) ?? app.projects[0];
}

