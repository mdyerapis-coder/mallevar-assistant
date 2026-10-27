# Mallevar Assistant Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the complete workstation assistant: conversations, knowledge, arbitrary qualified providers, genuine multi-harness orchestration, Codex WebGPT, local JEV, and permission-gated AlphaCode browser/desktop capabilities in the approved Ironbark interface.

**Architecture:** One Rust core owns durable state, policy, scheduling and processes. Bun workers adapt existing model SDKs and native engines; they never become a second scheduler. Tauri hosts a typed, conversation-first Vite interface, not a terminal-wrapper product.

**Tech Stack:** Rust 1.98.1, Tauri 2, Tokio, SQLite WAL/FTS5, TypeScript/Vite, Bun 1.4.3, OMP SDK 18.8.9, ACP SDK 1.8.0, Bubblewrap, system Secret Service, optional explicit Bitwarden references.

**Spec:** [approved design](../../design/DESIGN.md), [WebGPT selection](../../design/WEBGPT-COMPARISON.md), [AlphaCode assessment](../../design/ALPHACODE-ASSESSMENT.md), [approved visual](../../design/index.html). This repository contains the approved design bundle under `docs/design/`; the original planning deliverable remains untouched.

## Global Constraints

- The local core is the only scheduler and approval authority.
- No claim that every provider can run through every harness.
- No silent paid/provider fallback, context disclosure, approval bypass, fabricated tool output, or fabricated session continuity.
- Credentials stay outside the frontend, prompts, retrieval index, logs, and exports.
- One writer owns each worktree; shared edits go through one integration owner.
- No telemetry or public listener by default.
- Work stops when the workstation is off.
- No existing OMP/Codex/OpenCode sessions, credentials, project files, or global configuration are overwritten to make the new client work.
- Unknown is not supported. Every advertised feature must meet its gate before being presented as available.
- Original Ironbark imagery, native decoration, system fonts, Australian English; no slogans, neon chrome, ornamental badges, blurred glass, or bark textures behind working content.

## Review Focus

1. A repeated Send after reconnect must not start a second provider turn or worker: C1/P2/D2.
2. A denied or stale grant must reach the real sink before side effects, including AlphaCode, shell and browser: C2/C3/H1/H7/H8.
3. Project-private chunks must never reach a disallowed provider, tool or export: P3/P4/C5/W2.
4. A native session/provider pin must not drift during resume, account failure or cross-harness handoff: H2–H7/W1–W3.
5. A cancelled/crashed task with uncertain mutations must reconcile actual state, not replay automatically: C4/H1/W3/D4.

---

## Deliverable location and current evidence

These are plans, not product code. The target repository will be `/home/mason/Projects/ironbark-assistant`; it has not been created. All source paths below and in linked plans are relative to that future repository. Existing projects are donors/interfaces, not mutation targets except a deliberately pinned AlphaCode source patch within the new repository's integration build area.

Observed prerequisites: Bun 1.4.3; OMP 18.8.9; Codex 0.162.1; OpenCode 1.19.0; Claude Code 2.1.296; Hermes v0.19.0. `hermes acp --check` passed. OMP SDK 18.8.9 and ACP SDK 1.8.0 exist in npm metadata. Rust shims exist but no toolchain is installed. WebKitGTK/JavascriptCore 4.1 development pkg-config entries are missing. Bubblewrap 0.13.0 can launch unprivileged user/PID/network namespaces; this is not final containment proof. Root has 429 GiB free in the current check, contrary to stale older pressure memory.

WebGPT bridge is not running at `127.0.0.1:17841`. Existing OMP/WebGPT source pins 18.6.1 and references older Codex/bridge versions. Local JEV is responsive but the retained four-case smoke matched only one expected task label. AlphaCode was inspected, not installed; upstream ACP has no permission round-trip. None of these observations may be presented as product integration qualification.

Every command labelled **Expected** in these plans is a future verification target, not an output already obtained. Real provider/ChatGPT/desktop/external-action smokes require their exact point-of-risk approvals; exercise denied paths without creating external effects.

## Linked plans and execution order

| Plan | Tasks | Independently useful delivered behavior | Dependencies |
| --- | --- | --- | --- |
| [01 Local core](2026-10-12-01-local-core.md) | C1–C5 | Durable core CLI, policy, containment, recovery, backups | None |
| [02 Connections and knowledge](2026-10-12-02-connections-knowledge.md) | P1–P5 | Live chat, branching, cited retrieval and memory through core CLI | C1/C2; extraction/index workers need C3; turn cancellation needs C4 |
| [03 Harnesses and orchestration](2026-10-12-03-harnesses-orchestration.md) | H1–H8 | Real coding/review/verify across six native engines and bounded automation | C1–C4/P1; H8 needs H7; H6 joins qualified workers |
| [04 Codex WebGPT](2026-10-12-04-codex-webgpt.md) | W1–W3 | Qualified native WebGPT text/media/tools and recovery | C1–C4/P1/H1/H2; optional OMP route needs H1 |
| [05 Local JEV](2026-10-12-05-local-jev.md) | J1–J3 | Versioned local advisory judgments and held-out qualification | J1/J2 need C1/C2; J3 needs P4/H6 |
| [06 Desktop and release](2026-10-12-06-desktop-release.md) | D1–D5 | Approved native UI, all actual workflows and local packaging | D1 needs C1/P2; full acceptance needs every plan |

C1 and C2 establish shared interfaces before workers. After C3/C4, provider/knowledge and distinct harness adapters can proceed independently against those interfaces. Core scheduler/store changes have one integration owner. WebGPT and AlphaCode patches have separate pinned build ownership, not competing runtime authority. Begin the UI early after its real core bindings exist; do not defer actual surface verification until release.

## File responsibility map

- `Cargo.toml`, `rust-toolchain.toml`, `package.json`, `bun.lock`, `tsconfig.json`, `vite.config.ts`: reproducible workspace/toolchain, owned by C1 then amended only by consuming tasks.
- `crates/ironbark-core/src/contracts.rs`, `error.rs`: canonical public DTOs and typed errors; generate TS/schema outputs, do not hand-maintain a second schema.
- `crates/ironbark-core/src/store/`, `policy/`, `process/`, `net/`, `scheduler/`: sole durable authority. Detailed files and first owners appear in C/H tasks.
- `crates/ironbark-cli/src/main.rs`: real diagnostics/smoke-facing CLI for core operations, not a separate scheduler.
- `packages/contracts/src/generated.ts`, `schema.json`: generated wire contract, produced by C1's exporter `crates/ironbark-core/src/bin/export_contracts.rs`.
- `packages/adapters/src/`: versioned Bun stdio worker, SDK providers, native harness adapters, WebGPT and JEV. No core database access or policy writes.
- `integrations/alphacode/`: pinned source identity, reviewed patch series, build recipe, notices. Source checkout/build stays in `.sources/alphacode/` (ignored; never an active user's AlphaCode checkout).
- `integrations/webgpt/`: pinned upstream manifest, managed launch/containment recipe and notices. Do not wholesale copy alternate bridges.
- `apps/desktop/src-tauri/`, `apps/desktop/src/`: native client/socket projection and presentation modules. No database writes or worker launch from web content.
- `crates/ironbark-core/tests/`, `packages/adapters/test/`, `qa/fixtures/`, `qa/smoke/`, `qa/qualification/`: consumer-visible regression cases, disposable real-path smokes and versioned qualification evidence. No source-text/wiring tests.
- `docs/design/`, `docs/runtime/`, `CHANGELOG.md`: approved spec, implemented operating/security guidance and actual changes. Update after each successful smoke, not before it.

## Canonical contract v1 — implement in C1

Rust uses UUID newtypes serialised as strings (`ProjectId`, `ConversationId`, `BranchId`, `MessageId`, `JobId`, `NodeId`, `RunId`, `ActionId`, `GrantId`, `ArtifactId`, `DecisionId`); `Digest` is a validated lowercase SHA-256 hex string. Names below are the stable public contract; private struct layout may vary. Schemars + ts-rs export the serde tagged unions to TS. Monetary limits use integer micro-USD, never floating-point equality.

```text
RoutePin { connection_id: string, model_id: string, harness: HarnessKind? }
HarnessKind = omp | codex | opencode | claude | hermes | alphacode
CapabilityState = unknown | unavailable | unqualified | qualified
CapabilityObservation { name, state, implementation_version, evidence_digest?, observed_at }
ProjectPolicy { version: u64, project_id, allowed_connection_ids: string[],
  allowed_tools: string[], read_roots: string[], write_roots: string[],
  network_destinations: string[], local_only: bool, max_workers: u16,
  max_requests: u32, max_cost_microusd: u64?, spend_policy: free_only | paid_allowed,
  allow_unknown_cost: bool }
RunState = queued | ready | running | awaiting_approval | blocked | reviewing |
  verifying | paused | cancelling | cancelled | succeeded | failed
ActionSpec { id, run_id, project_id, kind, canonical_args: JSON, scope: JSON,
  destination?, policy_version: u64, expected_state_digest: Digest }
ApprovalGrant { id, action_id, action_digest, policy_version,
  expected_state_digest, expires_at, revoked_at? }
SessionRef { harness, native_id, native_version, project_id, cwd, resolved_route: RoutePin }
Citation { source_id, revision_digest, locator: {page?,section?,line_start?,line_end?,
  sheet?,cell_range?,bounding_box?,time_start_ms?,time_end_ms?} }
ContextPackage { digest, brief, constraints, excerpts: CitedExcerpt[],
  artifacts: ArtifactId[], unresolved_questions: string[], originating_sessions: SessionRef[] }
ResultManifest { run_id, native_session: SessionRef?, artifacts: ArtifactId[],
  checks: CheckReceipt[], acceptance_satisfied: bool, unresolved: string[] }
EventEnvelope { protocol: 1, seq: u64, request_id?: UUID, project_id?, run_id?,
  timestamp, payload: CoreEvent }
CoreEvent = turn_started | text_delta | text_replace | tool_started | tool_finished |
  approval_needed | state_changed | session_bound | artifact_added |
  judgment_recorded | usage_observed | error | turn_finished
```

C1 defines each event variant with identifiers/data needed by its owning task. A UI sees `text_replace` as replacement of one specified message, not append. `usage_observed` separates provider-reported tokens/cost from unknown estimates. New union variants require contract version negotiation, not permissive loss of security events.

Core socket API: `hello(protocol: 1, session_nonce: string)`, `request(request_id: UUID, command: CoreCommand) -> CoreReply`, `subscribe(after_seq: u64) -> stream<EventEnvelope>`. Commands include project/policy queries, provider registration/probe, conversation operations, knowledge ingestion/query, submit job/turn, decide approval, pause/interrupt/reconcile, import/export/backup and judgment requests. Mutation request IDs are durably unique before dispatch; subscriptions never dispatch. Core replies are `ok {value}` or `error {code,message,details}`, never secrets. `CoreClient.request<T>(command: CoreCommand, requestId?: string): Promise<T>` and `CoreClient.events(afterSeq: number): AsyncIterable<EventEnvelope>` are the Tauri/CLI client boundary, not general remote APIs.

Bun adapter RPC is newline-delimited JSON over inherited stdio, `protocol: 1`, correlation `request_id`, bounded frames. Parent creates and owns each process. Operations and TS signatures:

```ts
interface HarnessAdapter {
  describe(): Promise<HarnessDescriptor>;
  open(input: OpenSessionInput): Promise<SessionRef>;
  turn(session: SessionRef, input: TurnInput): AsyncIterable<WorkerEvent>;
  answerPermission(input: PermissionAnswer): Promise<void>;
  interrupt(session: SessionRef): Promise<void>;
  exportContext(session: SessionRef): Promise<ContextPackage>;
  close(session: SessionRef): Promise<void>;
}
interface ProviderAdapter {
  describe(connection: ConnectionConfig): Promise<ConnectionDescriptor>;
  stream(input: ProviderTurnInput, signal: AbortSignal): AsyncIterable<WorkerEvent>;
}
```

`OpenSessionInput = { run_id, project_id, cwd, route: RoutePin, context: ContextPackage, policy: ProjectPolicy, resume?: SessionRef }`; `TurnInput = { request_id, message_id, content: ContentPart[], allowed_tools: ToolDescriptor[], budget: ProjectPolicy }`. `WorkerEvent` carries the originating session/turn/tool IDs and one CoreEvent payload; core validates/enriches it before persistence. `PermissionAnswer = { native_request_id, action: ActionSpec, grant?: ApprovalGrant, decision: allow | deny }`. A grant is checked by the core and enforced before the native sink; the adapter cannot mint one. `HarnessDescriptor` includes version, transport, route compatibility, approval/cancel/resume/child observability, and capability observations. `ConnectionConfig` contains auth references only, allowed projects, protocol, endpoint and model inventory; workers receive task-scoped credential/broker leases separately, never via these DTOs.

## Common verification and failure policy

Each task supplies a focused command, expected failure before implementation, expected pass after, and a real-path smoke. Add permanent tests for meaningful boundaries/transitions only; do not add checks asserting implementation text, copied defaults or mock-forwarding. SDK/protocol fixture replays establish parser behavior, not live harness qualification. Keep each live probe's version, input, actual events, route identity, exit status and receipts.

Evidence locations are `qa/qualification/<subsystem>/<identity>/<case>.json`; large/private transcripts remain owner-only runtime artifacts outside git. Commit safe manifests/redacted fixtures only. A live blocked prerequisite remains named and blocks that capability; it is not a passing test or permission to substitute a stub. Never call the overall product complete while a required stage is unqualified.

At execution time create the repository and isolated worktree using the relevant skill, preserve unrelated work, and commit only task-owned files. All commits below refer to the future implementation repository. No implementation or dependency installation is performed during this planning handoff.

## DTO details and first-owner rules

C1 creates the canonical type module and exporter. Later tasks add their fully specified DTOs/commands there before implementing consumers, regenerate TS, and migrate every affected caller in the same task. Do not add executable placeholder operations to make a schema look complete; an unimplemented command returns an explicit unsupported operation and is never advertised. Interfaces below are the stable cross-subsystem shapes, not another manually maintained TS schema.

```text
ContentPart = text {text} | image {artifact_id,mime} | audio {artifact_id,mime} |
  attachment {artifact_id,mime,name}
ProviderMessage {role: system|user|assistant|tool, content: ContentPart[],
  message_id, tool_call_id?}
Capability = text|images|tools|structured_output|reasoning|embeddings|audio|
  transcription|image_generation|usage
ToolDescriptor {namespace,name,input_schema: JSON,required_capabilities: Capability[],
  approval_behavior: scoped|exact,connector_ref?}
CitedExcerpt {citation: Citation,text,project_id,disclosure: DisclosurePolicy}
DisclosurePolicy = LocalOnly | AllowedConnections {connection_ids} | ExplicitPublic
CheckReceipt {id,run_id,command: string[],exit_code: i32,started_at,finished_at,
  stdout_artifact?,stderr_artifact?,workspace_digest: Digest,executor_identity}
ModelInventoryEntry {id,display_name,advertised_capabilities: Capability[],
  observed_capabilities: CapabilityObservation[],context_limit?,pricing_ref?}
ConnectionConfig {id,protocol,endpoint,auth_ref?,allowed_project_ids,
  models: ModelInventoryEntry[],adapter_version}
ConnectionDescriptor {id,protocol,endpoint,models: ModelInventoryEntry[],
  capabilities: CapabilityObservation[],availability,auth_state}
HarnessDescriptor {kind,version,transport,compatible_protocols: string[],
  permissions: CapabilityState,cancel: CapabilityState,resume: CapabilityState,
  children: observable|opaque|disabled,capabilities: CapabilityObservation[]}
CoreStatus {protocol:1,instance_id,pid,last_seq,active_run_ids}
CoreSnapshot {last_seq,projects,conversations,runs,capability_observations}
```

`turn_started` carries run/message/native-turn IDs; `text_delta`/`text_replace` carry message ID plus text; `tool_started`/`tool_finished` carry native tool-call ID, namespaced tool, bounded arguments/result-artifact or typed error; `approval_needed` carries ActionSpec plus native request ID; `state_changed` carries old/new state and reason; `session_bound` carries SessionRef; `artifact_added` carries object ID/hash/provenance; `judgment_recorded` carries J1 JudgmentRecord; `usage_observed` carries route/native identity and optional reported tokens/cost/estimate semantics; `error` carries typed code/details and run correlation; `turn_finished` carries ResultManifest, not an assumed success flag. Security-bearing events with absent identifiers are invalid.

First ownership: C1 store/IPC/status/snapshot/contract; C2 action/task/grant/spend policy; C3 process/network leases; C4 state/checkpoints; C5 objects/backup/import; P1 connection/provider/auth; P2 conversation/turn/content; P3 source/citation/disclosure; P4 context packing; P5 memory/index; H1 native worker/worktree/results; H6 graph/roles/tools; H7 AlphaCode extension; H8 automation; W1–W3 WebGPT lifecycle/native/submission; J1–J3 judgment/qualification/advice; D1–D4 presentation/native dialogs/lifecycle. Leaf implementation DTOs (for example native SDK request aliases) are defined in the owning task's exact named file using that pinned SDK's types, not guessed exports.

Reject invalid locator ranges (page/line ≥1, end ≥start, nonnegative timestamps/boxes), nonmatching project/source identity and wrong MIME/capability combinations before context dispatch. Default spend policy is free_only with allow_unknown_cost=false; paid/unknown-cost routes require explicit owner policy. Neither catalog “free” labels nor JEV guesses satisfy billing qualification.

## Specification coverage and review record

Parent self-review maps every design section to deliverable owners:

| Design requirement | Owners |
| --- | --- |
| §1 complete local product/non-goals | All plans; D5 complete gate |
| §2 approved imagery/interface/accessibility | D1–D3/D5 |
| §3 single runtime/storage/ownership | C1–C5/D4 |
| §4 protocols, catalogs, native credentials, privacy/spend | P1/C2/C3/D3 |
| §5 genuine engines, roles/skills/tools, AlphaCode | H1–H8 |
| §6 selected WebGPT/native consent/containment | W1–W3 |
| §7 ingestion, revisions, retrieval, memory, branches | P2–P5/C5/D2/D3 |
| §8 existing local JEV/domain qualification | J1–J3/D3 |
| §9 graph/approval/workspaces/checkpoints/recovery | C2–C4/H1/H6/H7/H8/D4 |
| §10 XDG/redaction/events/CAS/backup/import | C1/C3/C5/P1/D1 |
| §11 every stage and end-to-end acceptance | D5 plus native per-engine gates |
| §12 observed limitations/owner decisions | Master evidence; W1/J2/D5 point-of-risk gates |

Self-review corrections: separated Claude and Hermes qualification; corrected H8/J3 dependencies; fixed locator fields and spend-policy semantics; retained AlphaCode permission bridge as required implementation work rather than assuming stock ACP safety. All six subsystem plans have five review-focus cases linked to concrete denial/error/transition tests or real native-surface scenarios. No product tests/builds have run; verification in this handoff covers source/prerequisite research and plan artifacts only.

Planning-artifact verification: all seven saved plans were read exactly (byte counts matched file sizes). The check found 29 unique tasks, 163 unchecked execution steps and 27 resolving local document links, with no structural failures. A deliberate missing link and missing Interfaces block were both detected by negative controls before checking the real artifacts. This proves the plan bundle's structure/link integrity, not implementation correctness, provider compatibility or production readiness.

## 2026-10-15: requested all-65 extension

The owner requested implementation of the original five unfinished acceptance outcomes plus all sixty suggestions. The complete proposed scope, current source gaps, shared ownership, safety prerequisites and separate acceptance criteria are in [the all-65 design](../specs/2026-10-15-all-65-design.md).

The owner approved the complete extension design. The [all-65 implementation index](2026-10-15-all65.md) links eight bounded subsystem plans, exact shared interfaces, dependency order and a separate code/proof mapping for every O01–O05/S01–S60 criterion. Existing historical plans/evidence remain intact and are not current runtime verification. Implementation proceeds without another plan/proceed confirmation; planning does not mark native interruption, live five-domain JEV, actual D2/D3/lifecycle, new upstream publication or final merge accepted.
