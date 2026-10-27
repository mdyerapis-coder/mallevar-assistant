# End-to-End Acceptance Checklist for Ironbark Desktop (Plan D5)

This document describes the **complete, verifiable acceptance workflow** for the Ironbark workstation assistant.
All steps assume a **fresh, clean installation** of the local package (using `packaging/local-install.sh` or official package managers).

## 1. Fresh Install
- [ ] Verify that the binary `ironbark-core`, `ironbark-cli`, and `ironbark-desktop` exist in the PATH.
- [ ] Verify that the core daemon can be started (run `ironbark-core` in a temporary directory) and emits `CORE_READY`.
- [ ] Verify that the CLI `ironbark-cli status` returns a valid JSON with protocol, projects, last_seq, abandoned_requests.
- [ ] Verify that the desktop entry `.desktop` file is installed (`/usr/share/applications/ironbark-assistant.desktop`).

## 2. First Launch (Desktop)
- [ ] Launch the Tauri desktop application (`ironbark-desktop`).
- [ ] Observe the connection banner: "connecting to core…" → "core live" after successful core connection.
- [ ] Verify that the status pane shows protocol, projects (0), and last_seq (0).
- [ ] Verify that the navigation rail includes "status", "projects", "events", "recovery" (if D4 recovery view is built).
- [ ] Ensure no errors in the console (stderr) and no unexpected crashes.

## 3. Core Start
- [ ] Verify that the core daemon is indeed running (process `ironbark-core` is alive).
- [ ] Verify that the core’s socket file (`/run/user/<uid>/ironbark-assistant/core.sock`) is present and owned by the user.
- [ ] Verify that the core can accept a `Status` command via the authenticated socket (using `ironbark-cli` if needed).
- [ ] Verify that the core can subscribe to events (core-event stream) and the desktop receives at least one `core-event` (e.b. `state_changed`).

## 4. Conversation Workflow (P2)
- [ ] Create a new project via desktop UI (menu → Projects → New Project). The core should respond with a new project ID and root path.
- [ ] In the conversation view, start a new conversation (composer). Submit a user turn (text only).
- [ ] Verify that the conversation appears in the conversation list and that the turn is reflected in the conversation history view (via CLI `conversation_history`).
- [ ] Verify that the core’s event stream includes `TurnStarted`, `TextDelta`, `TurnFinished` (or appropriate P2 events).
- [ ] Verify that untrusted source/tool text is escaped (no HTML, no script injection). Use browser dev tools to inspect the rendered HTML.

## 5. Knowledge Ingest (P3)
- [ ] Ingest a source (document) via desktop UI (Project → Sources → Ingest). Provide a file path (local test file).
- [ ] Verify that the core acknowledges the ingestion (returns a source ID).
- [ ] Verify that the source appears in the sources list (via CLI `sources_list`).
- [ ] Verify that source revisions can be retrieved (`source_revisions`).

## 6    . Retrieval (P4)
- [ ] Perform a retrieval query via desktop UI (Search / Retrieve). Query for a term that should match the ingested source.
- [ ] Verify that the retrieval result list is non‑empty and contains the expected source (or relevant content).
- [ ] Verify that the retrieved result is returned as a JSON payload (via CLI `retrieval_search`).

## 7. Memory (P5)
- [ ] List existing memories (desktop UI → Memory tab). Should be empty initially.
- [ ] Propose a new memory (text, provenance). The core must assign a memory ID.
- [ ] Verify that the memory appears in the memory list (`memory_list`).
- [ ] Decide on the memory (desktop UI: Review → Approve). Verify that the core updates the memory state (`memory_decide`).
- [ ] Forget a memory (delete). Verify that it is removed from the list.

## 8. Approvals (P5)
- [ ] Trigger an approval‑needed event (e.g., a tool call in a conversation). The core must emit an `ApprovalNeeded` event.
- [ ] In the approvals view, review the action spec and select **Grant** or **Deny**.
- [ ] Verify that the core’s event stream reflects the decision (`action_outcomes`).
- [ ] Verify that a granted action actually produces a tool result (if available) or proceeds accordingly.

## 9. Recovery (C4)
- [ ] Simulate a core crash (SIGINT to the core process). Wait for core to exit.
- [ ] Restart the core (via desktop UI “restart” or by launching a new `ironbark-core` instance).
- [ ] Verify that the core reconnects (desktop shows “core live”).
- [ ] Verify that any in‑flight requests (e.g., a partially completed conversation turn) are **reconciled** (not replayed). The recovery view should show observed workspace state and offer the four decisions (`AcceptObserved`, `ResumeReadOnly`, `ContinueFromCheckpoint`, `Abandon`).
- [ ] Test each recovery decision (where feasible):
    * AcceptObserved – the core should keep the observed state.
    * ResumeReadOnly – the core should become read‑only (no further mutations).
    * ContinueFromCheckpoint – the core should resume from the last checkpoint.
    * Abandon – the core should discard the current run and start fresh.
- [ ] Verify that the recovery UI does **not** present a “Retry” button for uncertain mutations (as per spec).

## 10. Clean Shutdown
- [ ] From the desktop UI, choose **Quit** (or send SIGINT to the core).
- [ ] Verify that the core exits gracefully (process terminated, socket file removed).
- [ ] Verify that durable state (database, checkpoints) is preserved on disk.
- [ ] Verify that reopening the desktop re‑establishes a connection to the same core (adopting an already‑running core).

## 11. Integration Verification (Codex, AlphaCode, WebGPT)
- [ ] Ensure that the native code signatures for external tool harnesses are present and correctly exposed.
- [ ] Verify that the desktop can launch external tools (if any) and that safety boundaries prevent arbitrary command execution.
- [ ] Verify that the core’s IPC authentication (UID, nonce) prevents foreign peers from issuing commands.
- [ ] Verify that all untrusted content (model output, source excerpts) is escaped and cannot inject markup or scripts.

## 12. Security & Safety Checks
- [ ] Verify that the core never writes untrusted user data into executable files.
- [ ] Verify that the core does not expose any shell, SQL, file, or HTTP capabilities to the webview.
- [ ] Verify that the desktop does not open arbitrary network connections from JS (except via explicit native safe opener).
- [ ] Verify that the core’s session nonce is never leaked to JavaScript.

## 13. Accessibility
- [ ] Ensure that the UI can be navigated with keyboard only (tab order, focus management).
- [ ] Verify that screen‑reader announcements are present (ARIA labels, live regions).
- [ ] Verify that high‑contrast mode is usable (CSS variables).

## 14. Packaging & Distribution
- [ ] Verify that the local‑install script (`packaging/local-install.sh`) builds both AUR and RPM packages without errors.
- [ ] Verify that the generated packages install the binaries, desktop entry, and licenses correctly.
- [ ] Verify that the `manifest.json` contains the correct version and component metadata.
- [ ] Verify that the `licenses/` directory contains the MIT license (or other applicable licenses).

## 15. Regression Tests (Run All Smoke Tests)
- [ ] Run the existing smoke suite (`bun test` for adapters, `cargo test` for core) to ensure no regression.
- [ ] Ensure that `bunx tsc --noEmit` passes (type checking).
- [ ] Ensure that `bun run desktop:build` passes (frontend build).
- [ ] Ensure that `cargo test --workspace` passes (core tests).

## Named Blockers / Expected Shortfalls
- **WebGPT Integration** – Requires a real owner account + interactive login. **Blockers**: No account exists; WebGPT live smoke is a named blocker. Report as such.
- **JEV Runtime** – The JEV runtime uses Qwen3-4B/8092 (not the pinned 0.5B/8091) — any JEV live gate fails closed on identity mismatch.
- **Native harness CLIs** – Present but untested/unavailable = Unavailable, not Qualified.
- **External Publication** – No external publication, no paid calls, no account login from these tasks.

## Final Note
Every acceptance item must be **observable, evidence‑backed, and repeatable**. Use the provided CLI, desktop UI, and system commands (as documented in the spec) rather than inventing ad‑hoc tests. The acceptance checklist serves as the **single source of truth** for the integration owner to confirm that the delivered workstation assistant meets all functional, security, and reliability requirements.

---
*Generated by the Ironbark implementation plan (Task D5) on 2026‑10‑13.*