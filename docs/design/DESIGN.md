# Mallevar Assistant — workstation client design

Date: 2026-10-12

Status: written design approved in the conversation; implementation planning is in progress. The visual direction was approved separately. The product runtime has not been implemented or connected. The preview is deliberately not a working agent client.

## 1. Intent and confirmed requirements

Build one workstation application combining the conversational usability of ChatGPT with the coding capability of OMP, a shared knowledge base, and complete multi-harness orchestration. It is not merely a chat UI wrapped around a terminal.

The user requested:

- Chat, project knowledge, coding, and orchestration in the same client.
- Connections for any of their providers, with truthful compatibility limits.
- Multiple genuine execution harnesses and coordinated agents.
- Operation on this workstation only. No hosted orchestration, remote runners, mobile client, or remote-control infrastructure is required.
- A less formulaic, less AI-generated-looking interface.
- The supplied Ironbark OS imagery and visual identity.
- Codex WebGPT as an additional connection, chosen after comparing GitHub implementations rather than assuming the existing bridge is automatically the best.
- The existing local JEV for judgment and determination inside the assistant.
- AlphaCode as an additional native harness, with useful browser/desktop capabilities integrated behind the same permission and lifecycle boundaries.

The user approved the revised Ironbark visual preview and this written design. Task-scoped autonomy was the assistant's stated interpretation of a short affirmative answer, not approval for unlimited access. Account/security changes, consequential actions and artwork redistribution still require their specific approvals.

### Product name and engineering calls

“Ironbark Assistant” is the working name used in the preview, not a claim that an OS-wide runtime already exists. Recommended: ship a standalone Linux desktop application that follows the existing Assistant → Roots → bounded-workers ownership contract without requiring an Ironbark OS installation or changing ForgeOS as part of this project.

### Non-goals and anti-goals

- No replacement operating system, public SaaS, cloud account, always-on remote service, or mandatory GPU.
- No rewrite or flattening of every harness into a common fake agent loop.
- No claim that every provider can run through every harness.
- No silent paid/provider fallback, context disclosure, approval bypass, fabricated tool output, or fabricated session continuity.
- No autonomous model training or self-modification of policy. JEV feedback can be retained for review; changing its model requires a separate controlled evaluation.
- No claiming parity with every proprietary ChatGPT feature. Equivalent workflows are provided where a connected model/tool supports them; unavailable capabilities are named, not simulated.
- No wholesale merge of unrelated GitHub applications just to accumulate features.

## 2. Approved interface

The approved design is a compact, conversation-first Ironbark workspace:

- Left: New conversation, Conversations, Tasks, Knowledge, project list, recent conversations, Settings.
- Centre: the active conversation or task/knowledge view, with readable source references and a persistent composer.
- Right: an optional task-details panel showing the current agents, dependencies, workspace, context, and permission scope.
- The composer selects model/provider separately from harness; “Chat only” makes tool-free conversation explicit.
- Tasks expand into dependency, agent, output, file-diff, review, verification, approval, and recovery views. These are working surfaces, not decorative status cards.
- Knowledge opens real sources and approved memory with provenance. Citations open the exact cited revision/page/section.
- Provider and harness management live in Settings, with a compact per-run override in the composer.

Visual treatment follows the existing Ironbark design contract: soot canvas, warm neutral surface steps, parchment text, small amber focus/selection indicators, cool informational states, modest corners, and fine separators. Use system fonts and native window decoration. No slogans, neon chrome, ornamental badges, blurred glass, or bark textures behind text, code, logs, forms, or approval details.

Original `ironbark-night.jpg` supplies the sidebar imagery. Original `ironbark-lock.jpg` supplies the quiet new-conversation surface. Do not regenerate, distort, or remove the artwork's background. The detailed maker mark is not a tiny toolbar icon.

Preview: `index.html` alongside this document; its relative `files/` images allow local viewing. Preview messages, documents, and running-task labels are sample content, not evidence that actual work happened. The live companion may serve the same design through its authenticated loopback URL.

Image provenance is established by the existing ForgeOS design documents, but redistribution rights are not established there. Local design preview is not permission to redistribute. Packaging the originals is gated on the owner's rights/attribution disposition; do not silently replace them with generated imagery.

## 3. Architectural choice

| Approach | Benefit | Cost/risk | Decision |
| --- | --- | --- | --- |
| Fork OMP into a desktop chatbot | Immediate coding capabilities | Couples conversations, knowledge, and other harnesses to OMP internals; upstream upgrades become expensive | Reuse OMP through an adapter, do not fork its agent loop |
| Fork an existing broad multi-agent UI | Existing screens and many integrations | Unknown orchestration ownership, permission semantics, migrations, and provider assumptions | Donor/reference only unless a concrete implementation beats the established local shell |
| Standalone Ironbark client with one local core and native adapters | Stable product state, shared knowledge, genuine harnesses, controlled boundaries | Requires a real scheduler, data model, and qualification of adapters | Recommended |

### Stack recommendation

- Tauri 2 desktop shell and Rust native boundary, reusing suitable ForgeDeck code rather than importing its entire OS/router dependency chain.
- TypeScript/Vite interface and typed presentation modules, following the existing web-based shell direction. Do not introduce a second frontend framework solely to recreate this mockup.
- Rust local core for durable state, approvals, scheduling, artifact ownership, process supervision, and IPC.
- Small versioned TypeScript/Bun adapter processes where required to reuse OMP SDK/native protocol code. These adapt engines; they do not create a second global scheduler.
- SQLite WAL for local metadata and transactional event history; content-addressed files for attachments, extracted content, checkpoints, and run artifacts.
- Existing local JEV service as a bounded loopback dependency. Do not start another competing llama-server or change File Steward's active model as part of ordinary assistant work.

One user-scoped runtime remains alive while tasks run, independent of a window's presence. Launch it with the app; optionally use a normal systemd user unit for lifecycle control. The core needs no root daemon, public listener, login-time activation, or remote-control access. Closing the last window offers “keep tasks running” or “stop tasks”; selecting stop observes actual worker termination. Work stops when the workstation is off. WebGPT full-mode connector traffic is a separately consented external transport, described in §6; it does not move orchestration to a remote runner.

### Ownership

| Component | Owns | Does not own |
| --- | --- | --- |
| Assistant UI | Conversation presentation, interaction, user decisions | Tool execution, provider secrets, approval truth |
| Local core / Roots boundary | Projects, job graph, permissions, budgets, state transitions, checkpoints, evidence | Model reasoning or claims of provider capabilities |
| Provider connection layer | Endpoint/protocol/auth reference, model inventory, capability/health observations | Filesystem permissions or orchestration policy |
| Harness adapters | Native sessions, tool/event translation, owned child processes, native permission callbacks | Cross-task authority, global memory, permission escalation |
| Knowledge service | Source revisions, retrieval, approved memory, citations, disclosure filtering | Treating retrieved instructions as user authorization |
| Local JEV adapter | Typed advisory judgments with provenance and qualification state | Approvals, proof of correctness, permission grants |

The local core is the only scheduler and approval authority. An LLM planner may propose a graph; deterministic validation checks cycles, dependencies, scope, tools, budgets, and compatibility before dispatch. Harness-native subtasks retain native behavior but operate under delegated limits; adapters surface observable children. If a harness hides its inner subtasks, mark them opaque and bound the parent rather than invent a complete agent tree.

## 4. Provider connections

Connections are protocol-based, not a hardcoded vendor list:

1. OpenAI-compatible Chat Completions.
2. OpenAI Responses, including required event/tool distinctions.
3. Anthropic Messages.
4. Gemini-native where needed.
5. Local Ollama and llama.cpp endpoints.
6. Native/custom extension connections, including the selected Codex WebGPT bridge.

Each connection records its stable ID, endpoint, adapter type/version, authentication reference, permitted projects, model inventory, observed capabilities, observation time, and availability. Capabilities include text, images, tools, structured output, reasoning controls, embeddings, audio, context limits, and usage reporting. Catalog claims and live qualification are separate fields. Unknown is not supported.

Reuse KeyHarbor's catalog and supported credential-reference mechanisms through a narrow integration. Do not import a mutable global coordinator as a second provider authority or scrape private harness databases as the application's canonical configuration. Existing harness configuration may be inspected/imported with a preview; writes/synchronization are explicit, reversible actions. Keep direct CLI use viable.

Credentials stay outside the frontend, prompts, retrieval index, logs, and exports. Prefer the workstation Secret Service or an explicitly configured Bitwarden reference. Native authentication sessions remain owned by their native client. Inject only the credential required for the selected connection; do not inherit the entire desktop environment or mount all user secret directories into a worker.

Per-project route policy includes local-only, allowed cloud connections, free-only or paid allowance, and limits. A provider outage never relaxes privacy or spend policy. No silent fallback to a different account/provider/model. Show the reason a route is unavailable and let the owner change the selection. Do not promise accurate cost where a provider reports no usage or billing data; mark it unknown and enforce request/concurrency limits instead.

## 5. Native harness integration

Initial scope is the installed OMP, Codex, OpenCode, Claude Code, and Hermes ecosystem. New harnesses use the same versioned adapter contract.

| Engine | Observed local interface | Design use |
| --- | --- | --- |
| OMP | `omp acp`, `--mode rpc`, `--mode rpc-ui`, native extension/provider SDK | Primary coding integration; use the native RPC surface for features absent from ACP |
| Codex | `codex app-server`, stdio/Unix/socket transports | Genuine native thread/turn identity, approvals, streaming, session operations |
| OpenCode | `opencode acp`, `opencode serve` | ACP/native server as needed, with explicit local binding and access control |
| Claude Code | streaming JSON input/output, host permission callbacks | Native session and approval adapter; no `--dangerously-skip-permissions` shortcut |
| Hermes | `hermes acp`; `hermes acp --check` passed imports during planning | Use native ACP after session/approval/cancellation qualification; do not use one-shot mode that bypasses approvals |

A terminal launcher is not a complete orchestration adapter. Full qualification requires streaming, result identity, native resume, permission handling, cancellation, error propagation, workspace scope, and child-process ownership. Retain an interactive terminal for native features without falsely labeling terminal screen scraping as structured tool execution.

### Adapter contract

Operations: describe capabilities and observed version; probe health; create/resume a session; submit a turn/job; stream versioned events; answer a native approval request using an authorized grant; interrupt/stop; export a bounded context handoff; collect actual results and artifacts.

Every run records the selected provider/model and harness/version. Explicit pins override preference, never policy. Invalid combinations are rejected before dispatch. Tool calls, outputs, approvals, native session references, usage, and errors retain provenance; normalization never invents missing fields.

Harness switching creates a new native session with an explicit context package: brief, relevant conversation/source excerpts, completed-work summary, constraints, artifacts, and unresolved questions. Preserve the original native transcript. Do not pretend that hidden reasoning, KV cache, or server-owned session state was transferred.

If nested harness/model fallback cannot be restricted to the project's allowed routes, the engine is not eligible for unattended use under that policy.

### Agent roles, skills, and tools

Provide one application-owned role registry for planner, researcher, implementer, reviewer, verifier, and user-defined specialists. Each role declares instructions, required capabilities, source/tool scope, allowed harnesses/routes, and delegated resource limits. Render harness-specific agent definitions through adapters; do not symlink one frontmatter/schema file into every harness or overwrite native built-ins and user-authored agents. Imports and generated changes are previewed with explicit ownership.

Skills, project rules, hooks, and extensions retain source identity and activation state. Treat imported instructions as untrusted configuration until the owner activates them; they cannot expand policy. An unsupported skill/tool stays visibly unavailable for that harness rather than becoming a silent approximation.

Use a scoped tool registry for MCP integrations and first-party knowledge/orchestration functions. Support qualified local stdio tools and explicitly approved remote tool endpoints without exposing the core as a public service. Each tool has an exact namespace/schema, connector/auth reference, required capabilities, and approval behavior. Native harness tools such as filesystem edits, terminals, LSP, browser, and desktop interaction stay with the executor that actually implements them. Only advertise tools authorized for the current task; preserve native failures and real receipts.

Workflows bind roles to graph nodes, not to one permanently hardcoded model. User-chosen role/harness/model pins remain visible and are honored subject to compatibility and policy. A role definition is not an approval grant.

### AlphaCode extension requested during implementation planning

Add `dragonked2/alphacode` at inspected commit `2a1d17156282e59fa811ceb58d9e28302d360fba` (1.0.79), not DeepMind's AlphaCode research system. It is a genuine Rust coding harness with native sessions, ACP, browser tools and Linux AT-SPI accessibility. Keep its daemon and tools behind a versioned worker adapter; do not import its swarm scheduler, global provider selection, memory or updater as competing authorities.

Its current ACP bridge lacks a client permission round-trip and can attach to an existing daemon without reconciling route identity. The implementation must add a small audited dispatch-time permission bridge to the pinned source, negotiate its version, isolate daemon/config/socket ownership, verify provider/model/cwd, and prove OS containment before unattended mutation. Unmodified ACP is not sufficient.

Keep telemetry and updates disabled in the managed worker; select the approved provider explicitly, never a silent free route. Browser and desktop tools require action-specific authority and live user presence for consequential operations. Qualify real KDE/Wayland accessibility actions; unsupported platform actions are unavailable, not simulated through unrestricted shell commands. AlphaCode's internal swarm stays disabled until delegated limits and observable child ownership are enforceable.

`ALPHACODE-ASSESSMENT.md` records source evidence and gates. Adding AlphaCode does not establish that the complete assistant already exists; it expands the full implementation and acceptance scope.

## 6. Codex WebGPT connection

Treat WebGPT as a special native model connection and Codex as a separately selectable execution harness. Its configuration and status must never be conflated with Codex OAuth/API access or an ordinary OpenAI Responses endpoint.

The existing local OMP adapter documents and implements this ownership path:

Assistant → OMP native session/tool executor → native Codex app-server → loopback ChatGPT Web bridge → authenticated ChatGPT session.

Native Codex supplies genuine thread/turn history; OMP supplies dynamic tool execution and its approval boundary. Existing source registers `codex-web` with API `codex-web-native` at `127.0.0.1:17841/v1`, starts native Codex, uses native dynamic tools, and rejects unknown server approval requests. Preserve this separation when adopting the best bridge implementation.

Recommended primary WebGPT execution route: the local core supervises a genuine Codex app-server worker using the selected upstream sidecar. OMP remains a genuine independent worker. Reuse the local OMP-to-native-Codex adapter only after its dynamic-tool, approval, native-authority, and version contract passes the coordinated qualification gate. Do not register this as a generic OMP OpenAI-compatible endpoint. Neither current upstream source nor historical local smoke records prove the combined installed versions work today.

### Required behavior

- Explicit bridge lifecycle, health, authentication-needed, connector-approval-needed, unavailable, and stalled states.
- Separate native conversation identity per client session. No accidental reuse across projects.
- Use the bridge's genuine native protocol contract, not a synthetic OpenAI wrapper or invented IDs.
- Advertise only supported models/features and validate actual installed versions. Do not hardcode the historical model list as current truth.
- In chat-only mode, tools are absent/denied. For coding, the selected qualified executor owns the tools and approvals.
- Streaming, images, native history, tool success/failure, tool denial, cancellation, and session isolation require real qualification before claiming support.
- Native provider/website consent remains a human decision. Do not automate login consent, bypass challenges, or assume a subscription permits every automated use. Account/provider terms must be reviewed before live activation.
- No fallback to another API route when the Web connection fails.
- Keep temporary chats as the privacy-oriented proposed default if the chosen implementation supports them; saved chats are an explicit owner choice.

### Full-mode setup and external boundary

The selected upstream's full tool mode requires ChatGPT Developer Mode, a named `Codex Native2` connector, account/workspace permission for tool actions, and OpenAI's outbound secure MCP tunnel. Its documented connector setup uses Authentication None and Allow all actions. These are material account/security changes; the application must explain the exact capability/destination scope and obtain separate interactive approval at setup. No connector, account setting, tunnel, trusted native hook, or default Codex route was changed in this session.

The outbound tunnel does not require opening an inbound workstation port, but it still carries remote connector traffic. This is workstation-hosted execution, not local-only inference or a wholly local transport. Keep connector approval distinct from native filesystem/process approval. Keep connector autoapproval disabled; a task-scoped native grant does not authorize clicking a ChatGPT consent prompt. Refusal or lack of account eligibility leaves full tool mode unavailable, not silently replaced by fake tool output.

Run the bridge under one lifecycle owner and a dedicated managed Codex configuration/auth scope. Upstream setup expects the built-in Codex `openai` provider identity and can modify route/hook settings; do not rewrite the user's global custom-provider configuration. Qualify isolated home, rollout, native authority, and hook handling. Bridge version strings alone are insufficient; pin the inspected source/release identity and native protocol schemas.

Ordinary upstream Responses requests lack a bridge-specific bearer secret. Origin checks and loopback binding are not authentication of all local processes. Define and qualify the raw bridge's local-peer containment/trust boundary; an authenticated facade alone does not secure a directly reachable unauthenticated backend. Do not claim hard containment until it is exercised.

Upstream composer predictions default enabled and can produce extra account traffic; the recommended managed profile disables them until the owner explicitly opts in. This is an engineering/privacy recommendation, not an applied setting.

### Current observed state

The loopback probe could not connect to `127.0.0.1:17841`; the bridge was not active at that address during this session. The local adapter pins OMP SDK packages to 18.6.1, while installed OMP reports 18.8.9. Its README references Codex 0.160.0. These are compatibility risks, not proof that the adapter is broken. No ChatGPT turn, login, bridge deployment, or tool round trip has been performed here.

Repository selection and merge rationale are recorded in `WEBGPT-COMPARISON.md` after source review. Use one canonical bridge and one application adapter; a merge must identify an exact compatible delta and license basis. Do not combine duplicate session managers or two browser-auth stores.

## 7. Shared knowledge and memory

Keep three distinct layers:

- Sources: user-selected files, attachments, documents, imported notes, and fetched references, with immutable revisions and provenance.
- Project memory: explicitly approved facts/decisions/preferences linked to their originating evidence and scoped to the project.
- Working context: transient run summaries and retrieval excerpts. It is not automatically promoted into permanent memory.

Ingestion supports Markdown/plain text, source code, PDF, HTML, and common office documents using real extractors. Image/OCR and audio/transcription use qualified local or approved provider tools; unavailable media handling is reported without fabricated content. Remote extraction/transcription requires the same disclosure policy as a model request.

Use lexical full-text search plus an optional local embedding index. Keyword retrieval remains available if embeddings are offline; do not call this semantic retrieval. Tag embedding model/dimensions/revision, reindex only affected sources, and cap CPU/storage usage. No mandatory vector database service is needed for a single workstation.

Apply project/access/disclosure filtering before ranking and before constructing context. A citation points to a source revision and page/section/line span, not merely a filename. Updating a source does not rewrite old conversation citations. Deleting a source removes active retrieval entries and derived chunks/embeddings; historical citations show an explicit missing/deleted-source marker rather than pretending the content is still available.

Conversation memory candidates are shown for approval, editing, rejection, or forgetting. No silent cross-project memory sharing. Retrieved text is untrusted data; it cannot grant tools, expand permissions, or override the owner's instructions.

Conversations support streaming, attachment provenance, search, rename/archive/delete, edits as new branches, response regeneration as sibling branches, and model changes with visible context treatment. Active tool effects are not undone by editing a prompt. Exports are owner-initiated and redact secrets; importing another harness's history does not create a native resumable session unless that harness explicitly supports it.

## 8. Local JEV judgments

Reuse the local runtime, not the failing paid TypeSafe/OpenRouter route encountered by semantic-search tools during this session.

Observed local service:

- File Steward HTTP facade: `http://127.0.0.1:8420/v1/decisions`.
- llama.cpp inference: `127.0.0.1:8091`; the existing service owns its lifecycle.
- Active model observed: `Qwen2.5-0.5B-Instruct-Q8_0.gguf`.
- Existing API supports `type: enum` with `item`, `choices`, and `criterion`; `type: noul` supports a binary question and threshold.
- Implementation uses constrained grammar, short completion limits, prefix caching, and token log probabilities.

### Role within the assistant

JEV provides quick typed advisory judgments: task intent, candidate retrieval relevance, handoff completeness, whether a task needs additional evidence, and selection among already-policy-permitted routes. Each domain must have its own validated prompts/labels and evidence-backed qualification. A small judgment model is not the code reviewer, factual verifier, or final planner.

Run deterministic checks first for known facts: provider health/availability, budget, exact user pins, path restrictions, capability compatibility, approval validity, test exit status. JEV is consulted only for semantic uncertainty; it cannot turn a failed check into success or authorize a disallowed option.

Normalize results as decision ID, domain/version, candidate set, input references/hash, model/runtime identity, choice, score semantics, latency, and qualification state. `available`, `invalid`, `abstain`, and `unqualified` are distinct. Invalid/truncated outputs, missing meaningful scores, unknown model state, and service failures do not become the first enum choice or an invented high-confidence answer.

The inspected existing implementation may coerce truncated enum output by prefix matching and falls back to a first option in some paths. Its enum confidence is derived from a first-token log probability, not a measured whole-label calibrated probability. Shared prefixes can appear with the same score. The binary prior-correction code was written for file-oriented examples. These source facts require a domain-qualified adapter; they are not a proven causal explanation for every wrong choice in the smoke.

### Observed smoke and integration gate

`jev-smoke.json` retains four live assistant-task enum requests and their responses. Results: implement → RESEARCH, review → RESEARCH, chat → CHAT, research → CHAT. One of four matched the expected label. HTTP elapsed times ranged from 164.0 to 202.6 ms for these four calls. This small sample demonstrates responsiveness and an unmet qualification requirement; it is not an accuracy/latency benchmark.

Do not enable automatic assistant routing with this unqualified configuration. Integration is still part of the full product scope: before enabling a judgment domain, establish representative positive/negative/ambiguous cases, evaluate wrong-choice and abstention behavior, choose domain thresholds from held-out results, record model/prompt versions, and prove unavailable/invalid decisions cannot dispatch a mutation. Do not change the shared File Steward model without explicit owner choice and regression evidence for its existing consumers.

When JEV cannot decide reliably, show the judgment as unqualified/abstained and ask the main planner or the owner for the semantic choice. That path remains explicitly subject to the same policy; it is not disguised as local JEV output. No paid judging service is used as a silent fallback.

## 9. Orchestration, execution, and recovery

### Job model

A job has a brief, acceptance criteria, selected project/workspace, allowed sources and destinations, allowed tools/routes, budget/concurrency limits, and a versioned task graph. Nodes contain role, harness/provider pin or allowed candidates, dependencies, input artifact references, output contract, and verification requirements.

Validate the graph before any execution. Parallelize truly independent work. One writer owns each worktree; shared edits go through one integration owner. Reviews are read-only. Independent evidence collection follows implementation before a task is accepted. A provider/model preference does not imply a permission grant.

Suggested initial concurrency is a configurable limit of two active model-backed workers, not a permanent product limitation. Larger graphs queue safely. Allow independent reads/reviews in parallel when resource and provider constraints permit. Record opaque usage rather than fabricating token totals.

### State semantics

Queued → Ready → Running → AwaitingApproval / Blocked / Reviewing / Verifying → Succeeded or Failed. Pause/resume and Cancelling/Cancelled are separate transitions. Native completion is not product success until specified acceptance evidence is present. Unknown external state after interruption is Blocked, not automatically retried or marked complete.

An approval is bound to exact action, project/path/destination, capability, policy version, and relevant artifact/commit digest. A changed diff, destination, expired grant, revocation, or resumed run cannot reuse a stale approval. Human consent is not inferred from model text, retrieved documents, or native worker self-approval.

### Workspace and operating-system enforcement

Git worktrees provide change isolation, not a security sandbox. Unattended mutation requires a qualified Linux isolation mechanism and native permission callbacks: bounded mounts, minimal environment/auth references, path containment including symlinks, network rules, and process ownership. Select a supported unprivileged namespace/container mechanism during implementation qualification; do not claim hard containment based only on a prompt or a worktree.

If a harness cannot honor the policy under the selected environment, refuse unattended dispatch and explain why. Do not secretly lower sandbox settings to make it work. Browser/desktop/screen/clipboard tools are explicit capabilities; native user-presence and platform approval requirements remain in force. No unrestricted AI root. A privileged action, if ever needed, uses a separate exact-scope native authorization boundary.

Shell tasks can make external effects even without a publish tool. Enforce destination/network controls rather than relying solely on tool names. Provider traffic belongs to the selected connection; tools do not inherit unrestricted network access.

### Recovery

Commit checkpoints after planning, before mutation, after mutation, after verification/review, and before a consequential action. Checkpoints link actual workspace/artifact/commit state to node/run identity; they are not a claim of exactly-once execution.

On restart, inspect real child processes, worktrees, and committed event state before resuming. Never repeat an uncertain write/publication simply because a completion event was missing. Require reconciliation for uncertain external outcomes. Read-only provider calls may retry only within the configured route, spend, and cancellation limits; mutating actions require an explicit recovery decision.

Cancellation terminates the owned process group, verifies termination, and preserves partial artifacts. A cancellation request alone does not produce a “cancelled and cleaned” success label. Deleting or archiving an active conversation first resolves its associated runs.

## 10. Local data and trust boundary

Use XDG locations beneath an `ironbark-assistant` namespace for config, data, cache, and runtime socket. Files are user-owned; sensitive directories/files use owner-only permissions. No telemetry or public listener by default. Native IPC uses a user-scoped socket and peer/session authorization; the core's own HTTP connections, if necessary, bind loopback and use authenticated access, origin checks, and request limits. Existing third-party sidecars have their own observed limits; §6 records WebGPT's unauthenticated Responses socket and external connector boundary rather than pretending they inherit the core's protections.

SQLite entities: projects, conversations/branches/messages, attachments, provider references, harness capability observations, jobs/nodes/runs, approvals, events, artifacts/checkpoints, source revisions/chunks, memory records, and JEV judgments. UI state is a projection of persisted events, not its own mutable source of execution truth.

Persist sequence numbers and stable identities so reconnection detects missing events and rebuilds the view without dispatching another task. Aggregate usage remains distinct from billing truth. Secret redaction runs before durable event/log persistence, not merely before rendering.

Attachments/source/artifact contents live in content-addressed storage; records link hashes, origins, and revisions. Size limits, incremental indexing, retention controls, and safe user-initiated backups prevent runaway storage growth. Backup uses a consistent SQLite snapshot and includes referenced objects; credential references are preserved but raw vault credentials are not exported. Restore does not automatically resume side effects.

Imports are additive and previewed. No existing OMP/Codex/OpenCode sessions, credentials, project files, or global configuration are overwritten to make the new client work. Future code made obsolete by a deliberately approved cutover is removed; no permanent duplicate authority or compatibility shim.

## 11. Full delivery sequence and acceptance

These are delivery stages of the complete product, not authorization to narrow it to a scaffold. Implementation starts only after this design is approved; the next deliverable is a file-level implementation plan.

| Stage | Delivered behavior | Required real evidence |
| --- | --- | --- |
| 1. Local core and approved shell | Genuine persistence, event reconnection, project selection, task state, credentials boundary, process supervision | Restart/reconnect without duplicate dispatch; permission denial produces no side effect |
| 2. Conversations and knowledge | Live provider conversation, branching/history, attachments, project-scoped retrieval/citations/memory | Provider responds; citation opens exact source; disallowed provider receives no private excerpts; source update/deletion is correct |
| 3. OMP coding | Actual files/tools/terminal/diffs and approval UX through native OMP | Approved edit lands in assigned workspace; denied/out-of-scope write does not; actual checks and tool failures appear |
| 4. Other harnesses and orchestration | Qualified Codex, OpenCode, Claude Code, Hermes and AlphaCode adapters, dependencies, parallel jobs, review handoffs and permission-gated browser/desktop capabilities | Real cross-harness implement/review/verify workflow; incompatible route denied; one failed worker does not claim sibling success; AlphaCode denial reaches actual sinks and real Linux automation is qualified |
| 5. Codex WebGPT | Selected canonical bridge with native ownership and truthful model/auth state | Real text/image/history/tool/denial/abort/isolation cases on pinned versions; login/consent remains human-owned |
| 6. Local JEV | Domain-qualified typed judgments inside assistant decisions | Held-out domain evaluation, abstention/error handling, provenance, and proof that JEV cannot grant permissions |
| 7. Desktop delivery and lifecycle | Native dialogs/accessibility, cancellation/recovery, backup/import/export, packaging | Actual desktop interaction; owned children stop; crash before/after mutation reconciles; clean restore and no credential leakage |

Every advertised feature must meet its gate before being presented as available. The overall product is not complete until all included stages are delivered; an unsupported/unqualified adapter must not be replaced with a mock success path.

### End-to-end acceptance scenario

From the approved interface: select a project and allowed provider; discuss a source with citations; approve a task; have local JEV make a qualified advisory classification; let OMP implement in a contained worktree; let another harness review; independently run required verification; inspect diffs and evidence; explicitly approve integration of the reviewed result. Repeat through the Codex WebGPT connection where its capabilities permit. Interrupt a run, restart the client, and reconcile/resume without losing history or duplicating mutation. Deny a consequential action and prove it never occurs.

## 12. Grounding and verification limits

Existing local foundations inspected:

- `~/Projects/ForgeOS/forgedeck/`: Tauri/Rust process/PTY and provider/model inventory implementation.
- `~/Projects/ForgeOS/docs/ironbark/design/roots-styling.md`: Ironbark palette, imagery, legibility, and ownership contract.
- `~/Projects/ForgeOS/docs/ironbark/design/assets/README.md`: supplied asset identity and unverified redistribution rights.
- `~/Projects/ForgeOS/docs/ironbark/architecture/roots-integration.md`: deterministic approval, bounded workers, checkpoints, and recovery boundary; specification, not implementation evidence.
- `~/Projects/keyharbor/keyharbor_core.py`: provider configuration/catalog and local judgment integration.
- `~/Projects/omp-webgpt-native-adapter/{README.md,package.json,src/index.ts,src/native-client.ts,src/session.ts}`: native WebGPT adapter ownership and historical version pins.
- `~/Projects/file-steward/{steward_jev.py,steward_server.py}` and the installed user service: actual local typed decision contract.

Exercised here: installed CLI help/interfaces; the live and saved browser mockup's navigation/settings, citation opening, task-panel toggle, WebGPT selection, and imagery load; exact copied-artwork hashes; local JEV health and four real decision requests; an unsuccessful loopback bridge probe; a separate native `openai-codex/gpt-6-astra` screenshot-review invocation that returned substantive feedback and recommended retaining the approved layout.

Not exercised here: end-to-end harness integration, WebGPT inference/authentication, native application packaging, filesystem containment, recovery, backup, provider interoperability, code review, production knowledge retrieval, or general-purpose JEV accuracy. No product source/configuration was changed or deployed. Source review of GitHub candidates is separate from live qualification.

### Builder check and remaining owner decisions

The design was reviewed inline against the builder-check questions before handover; `BUILDER-CHECK.md` records the review and remaining activation prerequisites. It is not an independent audit or runtime certification. Minor engineering choices are the recommendations explicitly marked above. The actual file/module breakdown, protocol bindings, supported isolation mechanism, retrieval implementation, and test commands belong in the next implementation plan, constrained by this contract.

The owner approved this written design; the next deliverable is the implementation plan. Artwork redistribution rights and live native-provider login/connector/tunnel consent are required at their respective packaging/activation gates, not guessed from local possession or this mockup. The earlier statement that the top-level session was already Astra was unsupported; a later separate native Astra review did return real feedback, recorded in `ASTRA-REVIEW.md`. It did not author the HTML, switch the top-level model, or activate WebGPT. Astra through WebGPT still requires account/model qualification; a UI label is not proof.
