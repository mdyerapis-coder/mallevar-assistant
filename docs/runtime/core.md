# Local core

The native daemon owns SQLite, the authenticated local socket, policy and the service
dispatcher. The implemented command surface covers C1 project/status/events/doctor/shutdown,
C5 objects (artifact read, additive import, snapshot backup, offline stopped restore,
conservative retention, capacity) and P2 conversation-tree operations (create, history,
rename, search, export, edit, queue turn, regenerate, archive, delete). Manual `run create`,
`run start`, checkpoint, inspect and reconcile commands are implemented. Core-owned live
provider launch and resumable native control remain incomplete: `run_interrupt` returns
`run_dependency_missing`. Native harnesses, provider accounts, JEV semantic qualification
and desktop release are not qualified by this core.

## Build and run

```sh
cargo +1.98.1 build -p ironbark-core -p ironbark-cli
./target/debug/ironbark-core
./target/debug/ironbark-cli doctor --json
./target/debug/ironbark-cli project create example --root /absolute/project/path
./target/debug/ironbark-cli events --after 0
./target/debug/ironbark-cli status
./target/debug/ironbark-cli shutdown
```

The project toolchain pin selects Rust 1.98.1 without requiring a global rustup default.
Startup prints `CORE_READY protocol=1` after opening the store and binding the socket.

## Command surface

Every named subcommand maps to a canonical `CoreCommand` and prints exactly one JSON DTO on
stdout; `artifact read` prints the raw artifact bytes. `--request-id` (global) is the durable
request identity for replay-safety. `backup restore` is the one offline operation: it runs
without a daemon or authentication and installs into the operator-selected XDG namespace.

| Subcommand | Command | Notes |
| --- | --- | --- |
| `status`, `doctor --json`, `shutdown`, `events --after N` | C1 leaves | `events` streams bounded pages until drained |
| `project create NAME --root ABS`, `project list` | C1 leaves | `--root` defaults to the CWD |
| `import preview --project UUID --file ABS [--kind native-transcript --origin STR]` | C5 | preview is a reviewed, additive digest |
| `import apply --project UUID --digest HEX` | C5 | publishes content, then the durable receipt |
| `artifact read --project UUID --id UUID` | C5 | raw bytes to stdout; bounded by the IPC frame |
| `backup create --destination ABS` | C5 | online snapshot via the authenticated daemon |
| `backup restore --source ABS --new-namespace` | C5 | **offline**; stopped, fresh namespace, never overwrites |
| `retention sweep` | C5 | conservative reference-aware CAS reclaim |
| `object-capacity --project UUID` | C5 | published CAS capacity (soft/hard) |
| `conversation create/history/rename/search/export/archive/delete` | P2 | `delete --run UUID` resolves active runs |
| `conversation edit --message UUID PARTS`, `queue-turn`, `regenerate` | P2 | the frame request id is the stable turn identity |
| `command '<CoreCommand JSON>'` | generic | raw wire envelope, machine-parseable |

`conversation queue-turn` and `conversation regenerate` route to the durable leaves using the
frame request id as the turn identity; replay returns the same run, never a duplicate turn.
They do not launch a provider — C4 claims and launches queued work separately. `run_interrupt`
returns `run_dependency_missing` because the core cannot observe native stop and reconcile
terminal state on its own.

## Storage and trust boundary

| Item | Location |
| --- | --- |
| Database | `$XDG_DATA_HOME/ironbark-assistant/core.sqlite3`, falling back to `$HOME/.local/share/ironbark-assistant/core.sqlite3` |
| State | `$XDG_STATE_HOME/ironbark-assistant`, falling back to `$HOME/.local/state/ironbark-assistant` |
| Runtime | `$XDG_RUNTIME_DIR/ironbark-assistant`; without that variable, `<state>/runtime/ironbark-assistant` |
| Runtime files | `core.sock`, `session.nonce`, `core.lock` |
| Content-addressed store | `<data>/objects` (project-owned CAS) |

App directories are 0700; database, nonce, lock and socket are 0600. Directory ancestry is
walked from held descriptors without following symlinks: every component must be
root/current-user-owned, with no group/other write access unless sticky. This permits safe
sticky `/tmp` and ordinary `/run/user` ancestry without chmodding XDG parents. Private files
are opened relative to checked directory descriptors and must be regular, current-user-owned
and single-linked. SQLite opens through a held parent descriptor retained with the checked
database file for its lifetime. A held `core.lock` in the data directory excludes a second
daemon before migration, even when runtime directories differ. The separate runtime lock
protects endpoint ownership.

Both peers check Unix credentials. The first frame must negotiate protocol 1 and present the
runtime nonce; the nonce is never returned by status/events. This boundary permits trusted
same-user clients. It does **not** isolate malicious same-UID applications or root. Untrusted
execution requires the separate worker-containment boundary (see `containment.md`).

Credential-bearing fields, headers and recognisable credential text are redacted before
durable serialization. Raw Cookie/Set-Cookie header values, including folded continuation
lines, are removed in full. Redaction cannot discover arbitrary secrets in unrestricted text;
callers must keep native credential material out of durable messages and action arguments.
Credentials are not public DTO fields or database lease records.

## Request identity and reconnect

A mutating request is accepted under a UUID and command digest before dispatch. Replaying the
identical UUID/command returns the committed reply, not another mutation. Reusing the UUID for
a different command is rejected. Project mutation, its event and final reply commit in one
SQLite transaction. Service commands (C5/P2) are routed by `CoreServices::dispatch` to their
leaf service; the frame request id stays the durable identity, so `complete_request` persists
the redacted reply and marks the request completed under the same id.

Replaying a completed Shutdown request returns its durable reply without stopping the
replacement daemon. Only freshly executed successful Shutdown requests signal termination.

Accepted requests without a committed result are abandoned on restart. They require explicit
reconciliation, not automatic retry. A caller must not interpret disconnection as evidence
that a side effect did not happen.

Event subscriptions are read-only and resume after the last completely decoded sequence.
Persisted sequence—not wall time—determines ordering. The store pages replay with bounded
frames; slow subscribers cannot block unrelated clients or silently drop events. Unstreamable
events are rejected before commit. Malformed/oversized protocol frames and extra command
fields are rejected.

## Offline restore

`backup restore --source ABS --new-namespace` is an owner-selected **offline** operation. It
does not authenticate against a running core and never calls `RuntimePaths::prepare` before
the associated restore API. The source is validated (manifest, object set, hashes, database
integrity and reference closure), installed into a private sibling staging directory, stopped
(nonterminal runs blocked, in-flight requests abandoned, grants revoked, conversation turns
dispatch-claimed), fsynced with a `restore.json` marker, then atomically no-replace installed
into the freshly selected namespace. An existing namespace — even one whose core has stopped —
is refused (`NamespaceExists`). It launches no worker and reacquires no native session.

Schema-1 backup, restore and retention use `store::schema::StorageSchema`. Every durable
table/column/index and reference variant is reviewed; unknown schema refuses collection
before unlinking. Only typed owned references root CAS bytes. Human JSON-looking text,
external file hashes and policy/model identities do not become object references.
Sanitization preserves control identities and nonsecret raw logprob tokens. Restore
disables imported work, dispatch, grants, native associations and enrollment/check
authority; original checkpoint CAS bytes remain historical evidence, not fresh checks.

## Observed verification

On this workstation the integrated Rust suite and the actual prebuilt daemon/CLI were exercised
under disposable XDG directories:

```sh
cargo +1.98.1 test -p ironbark-core -p ironbark-cli
cargo +1.98.1 clippy -p ironbark-core -p ironbark-cli --all-targets -- -D warnings
cargo +1.98.1 run -p ironbark-core --bin export_contracts
bun install --ignore-scripts && bunx tsc --noEmit --strict
bun qa/smoke/core-reconnect.ts
bun qa/smoke/worker-containment.ts
```

- 106 Rust tests passed: durability (16), approval boundary (17), backup/restore (22),
  containment (10), conversation tree (18), dispatch routing (7), network boundaries (16).
- 63 adapter tests passed: provider boundaries (31) and JEV strict decoding (29).
- Clippy (`--all-targets`) and the integrated `tsc --noEmit --strict` type check were clean.
- `core-reconnect`: actual daemon/CLI binaries — status, reconnect, identical replay,
  altered-command rejection, bad-nonce rejection, graceful shutdown, restart, durable SQLite
  and 0700/0600 private permissions.
- `worker-containment`: actual native process/namespace termination evidence for 9 proof
  scenarios (symlink/nonexistent escape, secret home, network allowlist, grandchild cancel,
  runtime-ancestor rename/substitution, live worker budget, ptrace/process_vm, core death,
  private fd3), plus symlink-mount/loader rejection.
- A live import → artifact read → online backup → **offline restore** round-trip returned the
  exact source bytes from a daemon started in the restored namespace, and a second restore
  into an existing namespace was refused with `NamespaceExists`.

That historical verification is not complete application, native harness, provider account,
JEV domain or release qualification. Manual run/checkpoint/inspection CLI operations now
exist; core-owned live launch, resumable interruption and live qualification remain unfinished.

## Run scheduling and crash reconciliation (C4)

A run is the durable unit of work. It is born `queued` and every state change is
applied through the transition table in `crates/ironbark-core/src/scheduler/state.rs`;
no worker event and no frontend label writes `runs.state`.

| Command | Effect |
| --- | --- |
| `run create` | Mints the job/node/run chain for one task text. Requires an existing project; the task is bounded and redacted. Returns `{run_id, node_id, job_id, state}`. |
| `run start` | Admits `queued → ready` through the table, records the native session, partial artifacts and the observed process identity (pid + boot start ticks), then `ready → running`. |
| `run checkpoint` | Captures a phase-tagged checkpoint (`planned`, `pre_mutation`, `post_mutation`, `post_verification`, `pre_consequential`) plus its manifest as a CAS object; returns the artifact id. |
| `run inspect` | Read-only crash inspection: durable state, latest checkpoint, the recorded process identity (live / start-time match / `reused`), pending mutation and uncertain actions, plus recommended decisions. |
| `run reconcile` | Applies one owner decision: `accept_observed`, `resume_read_only`, `continue_from_checkpoint` or `abandon`, under current authority. |
| `run pause` / `run resume` | Retains the native session and partial artifacts; a UI disconnect never pauses a run. |
| `run cancel` / `run observe_stop` | A cancel *request* only reaches `cancelling`; `cancelled` requires the observed stop receipt. |
| `run native_done` | Native completion parks the run in `reviewing`. Success requires an explicit acceptance backed by a passing post-verification checkpoint. |

Reconciliation rules that the code enforces:

- `accept_observed` is refused while any consumed grant has no durable outcome
  (`outcome_unresolved`), and refused unless the latest checkpoint is a passing
  post-verification checkpoint (`acceptance_evidence_missing`) — a native
  completion label is never a false success.
- `continue_from_checkpoint` refuses to reuse a grant that is stale under the
  current policy (`stale_grant`); the run parks in `awaiting_approval` for fresh
  owner approval instead.
- A mutation without a durable consumption receipt moves the run to `blocked`
  (`mutation_without_receipt`) and is not performed.
- Inspection never signals anything: a PID recycled by an unrelated process is
  reported (`reused: true`), never killed, and no uncertain mutation is replayed.

The recorded start time must be the value the core can observe for that pid
(`/proc/<pid>/stat` field 22, clock ticks since boot). A wall-clock value is
misread as a recycled PID.

## Run and checkpoint CLI commands

The scheduler lifecycle is exposed through typed CLI subcommands, each printing
exactly one JSON DTO on stdout (the core's own DTO — never a re-shaped copy):

| Command | Effect |
| --- | --- |
| `run create --project <id> --task <text> [--workspace <dir>]` | Creates the queued job/node/run chain and prints the `RunReceipt`. `--workspace` is recorded as an explicit note in the task text: the protocol's `RunCreate` carries `project_id` + `task` only. |
| `run start --project <id> --run <id> [--connection <id>] [--model <id>] [--harness <kind>]` | Admits the queued run and starts it; the pin flags build the `RoutePin`. |
| `run pause` / `run resume` / `run cancel` / `run observe-stop` / `run native-done` | The matching lifecycle transition. A cancel *request* never becomes a stop receipt; `observe-stop` carries the observed stop. |
| `run inspect --project <id> --run <id>` | Prints the `RecoveryReport` (`run_state`, `recorded_pid`, `pending_mutation`, `uncertain_actions`, `checkpoint`, `recommended`). |
| `run reconcile --project <id> --run <id> --decision <accept-observed\|resume-read-only\|continue-from-checkpoint\|abandon>` | Applies one reconciliation decision under current authority. |
| `checkpoint capture --project <id> --run <id> --phase <planned\|pre-mutation\|post-mutation\|post-verification\|pre-consequential> --manifest <file>` | Parses the manifest file as a `ResultManifest`, captures the checkpoint as a CAS object and prints the artifact id. |

The generic escape hatch remains available for anything not yet given a typed
subcommand: `ironbark-cli command --request-id <uuid> '<CoreCommand JSON>'`.

## Native adapter transport boundary

ACP partial events are delivered while the native turn is active. Denial without a native
reject option returns cancellation, never an allow option. Interrupt waits for the actual
native `cancelled` response; a local request is not cancellation evidence. Session control
checks the complete pinned session identity, including project, cwd, harness and version,
without trusting mutation of a returned reference.

Only an explicit native `end_turn` is success; missing or unknown reasons, refusal and
budget exhaustion fail. EOF/error rejects pending native RPC calls rather than allowing
them to expire as misleading request timeouts. A real isolated ACP executable completed
two interrupted partial-output turns in one process, then exited on close. This is adapter
transport evidence only: core `run_interrupt`, private managed bridge, live compatibility,
native pause/resume and owned partial-artifact persistence are separate acceptance gates.
