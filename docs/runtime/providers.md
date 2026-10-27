# Providers and judgment boundaries

Provider invocation happens in the Bun adapter worker, never in the core. The core
registers connections, owns credential leases, and decides policy; the adapter is a
transport that executes one pinned route and reports back. This document describes the
implemented boundary, not a live-provider qualification.

## Connection pinning

`ProviderRegistry.resolve` refuses a dispatch unless:

- the connection exists and its protocol matches the route,
- the route carries no `harness` override (a provider route is not a harness route),
- the connection id is in the policy's `allowed_connection_ids`.

Known SDK protocols map to fixed SDK API descriptors (`SDK_APIS`); a `custom`
protocol requires an explicit activation version, otherwise dispatch fails with
`custom_activation_required`. An unknown protocol is `unsupported_protocol`. There is
no registry-level fallback to another account, provider or protocol.

## Exact protocol, no fallback

The OMP-SDK adapter (`providers/omp-sdk.ts`) enforces the transport contract:

- **Credential lease, not key.** The adapter receives a `credential_lease_id` and
  resolves bytes through a `CredentialResolver` on the private fd 3 binary lease
  channel. Raw keys never appear in the route, the worker environment or a log. An
  empty or missing credential resolves to a no-auth sentinel rather than an implicit
  account.
- **One dispatch per attempt.** On the first failed request the adapter aborts the
  dispatch; `providerRetryWait` throws `retry_requires_core_dispatch`. The SDK's
  separate transport-retry layer is disabled by construction. Only the core may
  re-dispatch, under a fresh request identity.
- **Controller composition.** The adapter's `AbortController` is composed with the
  SDK's derived watchdog signal (init or request signal) via `AbortSignal.any`, so
  owner cancellation and the first-event watchdog both abort the fetch; replacing
  one with the other is a bug the boundary exists to prevent.
- **Output ceilings.** Completed-line and cumulative upstream/output/producer
  backlog limits are enforced on the streamed result; a ceiling breach aborts the
  turn as failed rather than unbounded growth.
- **Exact replay.** A native replay is matched one-to-one against canonical
  `tool_calls`, including JSON arguments; a mismatched replay is a protocol error,
  not a silent drop.

A turn ends in exactly one of `succeeded`, `failed`, or `cancelled` — the distinction
between an owner cancellation and a provider failure is preserved in the terminal
event.

## JEV judgment boundary

The local JEV client (`jev/client.ts`) is a judgment **transport**, not a judge:

- **Strict raw decoding.** Responses are decoded from raw JEV frames; undecodable or
  truncated output is an error, never a coerced label.
- **Explicit abstention.** The client preserves the JEV's own abstention state. A
  judgment the model does not make is reported as unavailable, not defaulted.
- **Abort on every exit.** Every exit path (cancel, timeout, error, success) aborts
  the in-flight operation controller and clears siblings; the abort reason is
  captured before the `finally` re-asserts abort, so the terminal state is honest.
- **Metadata-only identity.** Client metadata identifies the endpoint; it does not
  qualify a semantic domain. Domain qualification requires held-out
  positive/negative/ambiguous cases, which this client does not perform.

### Local runtime discovery and holdout evaluation

Calibration, holdout evaluation and actual judgments share the client's strict
identity probe. It reads File Steward's `/api/status`, accepts only an exact
`http://127.0.0.1:<numeric-port>/v1` local runtime, and checks that runtime's
`/health` and `/props` against the facade's model path and active model ID.
It never rewrites facade responses or substitutes another provider. The runtime
base URL participates in the identity digest. Native llama.cpp quantization
metadata may be a nonempty string (for example `Q8_0`) or an integer.

The two-second end-to-end deadline, exact-code decoder, before/after identity
checks and explicit invalid/unavailable/abstention states are unchanged.
`available` is raw evidence, not permission or semantic qualification.

Fixtures live under `packages/adapters/qa/fixtures/`; the generator is in the
`jev/` subdirectory. Validation preserves each row's top-level class, rejects
unknown classes, duplicate IDs and repeated content (including across files or
splits). The dataset digest covers validated payloads, not just holdout IDs.
**The existing generated fixtures are diagnostic only and are now rejected**:
they repeat semantic inputs across splits, and independent review found inverted
evidence/handoff labels. Regenerating the same rows does not repair those defects.
Independent, reviewed, versioned scenarios are required before these commands
can establish qualification:

```sh
bun packages/adapters/qa/qualification/jev/calibrate.ts --domain all
bun packages/adapters/qa/qualification/jev/evaluate.ts --domain all --identity <observed-digest>
```

Freeze the observed digest before calibration/holdout. Available choices must
match that digest. The evaluation output retains per-row IDs, classes, expected
labels, choices, states and latency, without raw task text. A successful
evaluator exit means it produced reports, not that domains passed. The core
independently re-derives each qualification decision on persistence.

On 2026-10-15 the actual client observed Qwen2.5-0.5B-Instruct-Q8_0 on
`127.0.0.1:8091`, build `b11559-f2cc7282c`, and returned `REVIEW` for the
synthetic review smoke in 432 ms. The subsequent full run measured 1,000
calibration and 1,000 holdout rows before duplicate-content rejection was added;
all five domains were blocked. Those outputs are quarantined diagnostic
observations, not independent held-out estimates.

The diagnostic run also exposed a broken TypeScript binomial CDF and a Rust
tail-renormalization error. Both now accumulate the CDF in stable log space;
independent exact-combinatorial reference cases cover both tails. Core
persistence counts wrong answers as a subset of accepted choices, not twice.
Old numerical bounds must not be reused.

Calibration now emits a version-2 diagnostic artifact. Its `selection` records
all attempts, raw available choices, final accepted/wrong counts, empirical
error and each class's corresponding population. Equal raw-logprob groups are
indivisible: fitting stops at the first unsafe full group, then recomputes
counters from the final threshold. Missing/nonfinite/positive logprobs,
nonchoices and unavailable states cannot be selected. Empty selection has a
null threshold/error rather than an invented confidence.

The artifact binds domain/version/prompt/dataset/calibration/runtime identity
and its canonical content digest. Existing unreviewed v1 datasets have
`review_freeze_digest: null`; this is not a signed or qualified calibrator.
Auto-selection remains unavailable: evaluation still does not load/apply a
verified frozen rule, whole-run identity continuity remains outstanding, and
no domain is qualified. An isolated actual-CLI smoke with deterministic
transport verified19 selected choices from22 attempts; it measured population
semantics, not live model quality.


## Observed verification

63 Bun tests pass across `provider-boundaries.test.ts` (31) and `jev-decode.test.ts`
(29): protocol pinning, credential-lease resolution, first-failure abort, no-fallback
behaviour, output ceilings, exact replay matching, strict JEV decoding and
abstention preservation. These are boundary tests against fixtures and transport
shapes. **No live provider account, native harness or JEV domain is qualified.**
Live qualification requires owner-approved connections and is outstanding.
