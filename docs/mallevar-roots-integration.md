# Mallevar Assistant ↔ Roots integration boundary

**Date:** 17 October 2026. **Status:** proposed architectural reconciliation, not implemented API or release acceptance. **Current product name:** Mallevar Assistant. **Existing repository/binary names:** `ironbark-assistant`, `ironbark-core`, `ironbark-cli` (retained for backwards compatibility).

## Observed source and immovable constraints

This repository contains a single Rust daemon, SQLite store, authenticated Unix IPC, policy engine and service dispatcher. `crates/ironbark-core/src/contracts.rs` defines `ProjectId`, `JobId`, `NodeId`, `RunId`, `ActionId`, `ApprovalGrant`, `ProjectPolicy`, `RunState`, `RoutePin` and `CapabilityObservation`. `packages/contracts` is generated from Rust. Bun workers implement provider/JEV **transport**; they have no database or policy authority. `crates/ironbark-core/src/dispatch.rs` uses durable request identity with fail-closed in-flight reconciliation on interrupted accepted commands. The executable and current on-disk names must NOT be mass-renamed under a documentation change.

Implemented/per-component tests are recorded in the repository changelog. **Not yet qualified:** C4 run scheduling, live provider launch, live AlphaCode/JEV semantics and an installed desktop/runtime release. The dedicated Mallevar OS convergence documentation still describes Roots as the intended durable workflow engine and a historical Python/SQLite proposal. That is an unresolved architectural overlap.

## Proposed single-authority decision — review before coding

**Default design:** the existing Rust core owns the sole durable task/run/approval/event store and enforces policy. `Roots` is the user-visible orchestration experience, task graph planning/execution module and dashboard, calling versioned core APIs; it is **not a second authoritative Python SQLite scheduler**. The owner may explicitly select a separate Roots engine only by first demonstrating compatible migration, one-writer invariants, event replay, recovery and rollback in an approved architectural decision. Do not start both schedulers in parallel.

This proposal combines preserved Roots requirements (DAGs, worker registry, checkpoint/restart, approvals, graph revisions) with the working Rust security and storage boundaries without throwing either away. No new executable code is implied by this document.

### Proposed interfaces (not shipped contracts)

```
Mallevar Assistant (conversational intention)
    -> typed task proposal and capability requirements
Roots UI / graph projection (no authoritative writes)
    -> authenticated Unix IPC commands
Mallevar Rust core (one Store, schema ownership, run schedule, approval and event ledger)
    -> explicit policy/permission grant + isolated bounded worker
Provider adapters (OMP/local/custom, JEV transport) -> events and typed results
```

1. **Authoritative ID:** use existing `ProjectId` / `JobId` / `NodeId` / `RunId` typed IDs. Stable request IDs remain idempotency keys. Task/run/step event IDs must never be regenerated independently by UI or a mobile adapter.
2. **Approval:** one-time `ApprovalGrant` must continue to bind canonical action digest, policy version, observed state digest, expiry and revocation. A proposal is never executable permission; unknown fields and unrecognised schema versions fail closed.
3. **Scheduling:** only Rust-owned transitions may persist states; allow bounded graph revisions, dependency checks, per-node concurrency and budgets, timeouts, user cancellation, pause/resume and post-crash reconciliation. Do not convert `RunInterrupt` stub into a success claim.
4. **Transport:** provider registry preserves route pinning, credential lease isolation, single dispatch attempt, no automatic paid/provider substitution, cancellation propagation and output ceilings. `omp-sdk.ts` imports a library adapter; it is not evidence that engineering OMP should own runtime state.
5. **CLI compatibility:** keep `ironbark-core`/`ironbark-cli` commands, socket and data paths until consumers are inventoried and an explicit backward-compatible `mallevar-*` alias/migration tests exist.
6. **Cross-platform:** Mobile may request user-authorised actions via typed IPC/adapter, never bypass host policy or claim owner root privileges as agent privileges. Native platform permissions remain platform-owned.

## Required executable acceptance before runtime convergence

- Strict schema/version rejection, generated Rust ↔ TypeScript contracts identical at pinned SHA; deny unknown/hostile commands; ability to migrate or reject an older protocol with explicit error.
- Real graph execution, cycle/missing-dependency rejection, exactly-once persisted state transitions, duplicate requests and replay/restore after core crash.
- Exact-action grant creation, single-use/expired/revoked denial, stale state and policy mismatch tests; independent worker cannot self-approve or promote.
- Worktree process isolation, malicious paths/links, source-root escape and binary lease boundaries, user denial, cancel, timeout and crashed worker containment.
- Credential never appears in run ledger/logs, provider transport pinned with no implicit fallback, measured local CPU inference under network outage.
- Real Roots UI ↔ daemon launch/status/events/cancel/restart paths; full offline-first multi-user and platform boundaries; integration with OS packaging and cleanup.
- Install/reinstall/update/rollback on disposable OS guest while legacy binary/data/client paths keep functioning.
- Record exact source SHA, hardware, dependency versions, artifact hashes and independent test results. Fail the release gate until qualified, even if all unit tests pass.

## Required code review before merging this proposal into runtime design

Review with `mdyerapis-coder/ForgeOS/docs/mallevar/identity-and-compatibility.md`, historic `docs/ironbark/architecture/roots-integration.md`, `docs/ironbark/status/release-gates.md`, and `mdyerapis-coder/agent-forge` surviving source. Confirm which repository owns executable Roots and where the latest unpushed UI implementation lives. There is no approval here to archive Agent Forge or force a Rust/Python rewrite.
