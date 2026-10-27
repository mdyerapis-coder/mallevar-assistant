# Ironbark Assistant — shared integration context for implementation agents

You are implementing one task of the Ironbark Workstation Assistant plan. The parent (integration
owner) owns the shared files and verifies + commits each wave. Read this, then your task brief.

## Repo / paths
- Worktree (work here): `/home/mason/Projects/ironbark-assistant/.worktrees/build`
- Rust core crate: `crates/ironbark-core`  (lib name `ironbark_core`)
- CLI crate: `crates/ironbark-cli`; desktop: `apps/desktop`; TS adapters: `packages/adapters`
- Generated TS contracts (already include the new DTOs): `packages/contracts/src/generated.ts`
- Plan docs (your task's spec is there): `/home/mason/Documents/Ironbark-Client-Design/docs/superpowers/plans/`
- All contract DTOs (RunState, Digest, SourceRevision, CitationView, MemoryCandidate,
  QualificationRecord, AdvisoryOutcome, RecoveryReport, CheckpointPhase, DeletionReceipt,
  EmbeddingModelIdentity, JudgmentDomain, DisclosurePolicy, ContextPackage, …) are ALREADY
  defined in `crates/ironbark-core/src/contracts.rs`. **Do not add or edit DTOs there.**

## Rust conventions (copy these)
- `use crate::contracts::*;` `use crate::error::{CoreError, Result};`
- Errors: `CoreError::new(code, message)`; `Result<T>` is the crate Result. Codes are snake_case
  strings (e.g. `"illegal_transition"`, `"invalid_source"`, `"storage_corrupt"`, `"unsupported"`).
- DB access — the whole core shares ONE dedicated SQLite thread behind `Store` (never run SQL on
  a tokio worker, never hold a lock across await):
  - `Store::open(&Path) -> Result<Store>` (async)
  - `store.with_connection(move |conn: &mut rusqlite::Connection| -> Result<T> { … })` (async, read/tx)
  - `store.with_transaction(move |tx: &rusqlite::Transaction| -> Result<T> { … })` (async, IMMEDIATE + commit)
  - `store.append(run_id: Option<RunId>, event: CoreEvent) -> Result<EventEnvelope>` (async, durable event)
  - `store.accept_request(id: Uuid, command: &CoreCommand)` / `execute_accepted` / `complete_request`
- Tables already exist (see `src/store/migrations.rs`): `runs`, `nodes`, `jobs`, `checkpoints`,
  `sources`, `source_revisions`, `chunks`, `embeddings`, `memory`, `judgments`, `events`,
  `artifacts`, `requests`, `actions`, `grants`, `action_outcomes`, `conversation_turns`, …
  Column lists are in migrations.rs — read the exact table you persist to before writing SQL.
- A service is a `#[derive(Clone)]` struct holding `store: Store` (and `cas_root: PathBuf` /
  `ObjectStore` when it needs CAS objects). All public methods `async`. Construct on demand in
  dispatch — do not register a global.
- IDs: `Uuid::new_v4()`; digests: build a `Digest` from a hex sha256 (see existing `Digest::from_bytes`
  or `crate::store::objects` for the helper). `#[ts(type = "number")]` on `i64`/`u64` unix-ms fields is
  already handled at the DTO layer — your code just uses the DTOs.
- `Digest` is `pub struct Digest(String)` with a `from_hex`/`from_bytes` helper — check `contracts.rs:24-39`.

## Test + smoke conventions
- Unit/integration tests: `crates/ironbark-core/tests/<name>.rs`, `#[tokio::test]`. Build a temp
  store in a `tempfile::tempdir()`: `let store = Store::open(&tmp.path().join("core.db")).await?;`
  Then exercise the real service + real SQLite. Assert the SPEC's named assertions (they are
  behavioural, e.g. "old citation still exact after v2", "failed source publishes 0 chunks").
- Offline smoke: `qa/smoke/<name>.ts` (Bun). It drives the REAL prebuilt CLI — it never installs,
  builds, fabricates worker events, or uses a canned provider. Pattern: `Bun.spawn([CLI, "command",
  "--request-id", id, JSON.stringify({type:"…", …})])`. The CLI is at `target/debug/ironbark-cli`
  (env `IRONBARK_CLI_BIN`). A new `CoreCommand` variant reaches the CLI generically via
  `command <json>`, so a smoke can call it without new CLI subcommands. Write the smoke to the
  exact fixtures the spec names; it must pass offline where possible.
- Reference an existing test/smoke for style: `tests/conversation_tree.rs`, `tests/durability.rs`,
  `qa/smoke/conversation-tree.ts`.

## Live-gate reality (named blockers — do NOT fake)
- JEV runtime model is now `Qwen3-4B` on port 8092 (the pinned `Qwen2.5-0.5B`/8091 is gone). JEV
  qualification must fail closed and report the model-identity mismatch; the plan forbids switching
  the model. Record truthful blocked/unqualified state — never a green availability badge.
- Native harness CLIs present (codex/opencode/claude/hermes shims). Adapters must report real
  `describe()` capability state; a capability that is untested/unavailable is `Unavailable`, not
  `Qualified`. Live-adapter smokes that need an approved provider/connection are run by the
  integration owner — you write them to be runnable, but do not claim a live pass.
- No external publication, no paid calls, no account login from these tasks.

## HARD rules (the plan's global constraints)
1. **Do NOT run `git commit` / `git add`.** The integration owner commits per green wave.
2. **Do NOT edit shared files:** `contracts.rs`, `lib.rs`, `dispatch.rs`, `migrations.rs`,
   `ipc.rs`, `Cargo.toml`/`Cargo.lock`, or another task's module. The owner wires your `mod` +
   dispatch arm after you land. (Your own module's `mod.rs` and files inside YOUR module dir are
   yours; a `tests/*.rs` file and a `qa/smoke/*.ts` file named for your task are yours.)
3. **No stubs / placeholders / `TODO: implement`.** An unimplemented command must not be advertised
   as working. Real behaviour or an explicit typed error.
4. **No fabricated evidence.** Tests assert real store/FS/process state; smokes hit the real
   binary/endpoint. Never a canned model reply, never a fake receipt.
5. Match existing code style (line length, doc comments, error codes). Read the module you're
   extending before writing.
6. You MAY run `cargo test -p ironbark-core --test <your-test>` and `bun test <your-test>` to
   check YOUR work in your own scope, but you will not be the one to prove the whole build green —
   the integration owner does the cross-task `cargo build` + all-test pass at integration. Keep
   your code compiling against the current tree.

## What to return (final message)
- Exact list of files you created/modified (paths).
- The `pub` service struct(s) + method signatures you exposed (so the owner wires dispatch).
- Any NEW `CoreCommand` variant names you need the owner to register in `dispatch.rs`
  (`is_service_command` + a `perform` arm) and the reply DTO each returns.
- Which spec tests/smokes you implemented and their current status (pass / needs-integration).
- Any named blocker you hit (missing tool, live gate, model mismatch) — report it, don't hide it.
