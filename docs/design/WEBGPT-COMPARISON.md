# Codex WebGPT repository selection

Date: 2026-10-12. Source comparison for the Mallevar Assistant design; no code merge, bridge deployment, login, provider inference, build, or test has occurred.

## Decision

Recommend **miuuyy/codex-chatgpt-web as the canonical WebGPT transport candidate**, retaining the existing OMP native adapter's separation between model/session ownership and tool execution. Qualify the inspected upstream, native Codex, and installed OMP versions together before activation.

Use **chicogong/codex-chatgpt-web-orchestrator only as a bounded orchestration/evidence donor**. Its duplicate-submission, route-evidence, recovery, and receipt concepts fit the local core, but its assumed backends and narrow mode families do not implement a complete multi-harness client.

**Do not merge the repositories wholesale.** They implement different layers or competing execution loops. A merge would create duplicate conversation ownership, permission boundaries, browser profiles, and failure semantics without proving interoperability.

## Candidates inspected

Commit-pinned links below identify source review, not runtime certification. Candidates reported `main` and were not declared GitHub forks of miuuyy. Absence of a declared fork relationship does not prove independent authorship or absence of copied code.

| Repository | Inspected revision | License evidence | Decision and reason |
| --- | --- | --- | --- |
| [miuuyy/codex-chatgpt-web](https://github.com/miuuyy/codex-chatgpt-web) | `0b6087c5307c7a58adda9c420358f3f70d2035f5`; package version 6.1.8 | [MIT](https://github.com/miuuyy/codex-chatgpt-web/blob/0b6087c5307c7a58adda9c420358f3f70d2035f5/LICENSE), plus dependency notices | Preferred transport candidate: actual Responses server, browser adapter, native tool broker, session/compaction, and cancellation paths |
| [a252937166/codex-chatgpt-web-bridge](https://github.com/a252937166/codex-chatgpt-web-bridge) | `85cc3d01c99bc49be6baf039db556e82bbbe33d2` | [Apache-2.0](https://github.com/a252937166/codex-chatgpt-web-bridge/blob/85cc3d01c99bc49be6baf039db556e82bbbe33d2/LICENSE), NOTICE | Real alternative implementation, not an additive merge: replay-per-request, one JSON action, reduced session/media/streaming semantics |
| [chicogong/codex-chatgpt-web-orchestrator](https://github.com/chicogong/codex-chatgpt-web-orchestrator) | `0781dc2f18d853b77c546d5d59c6bed1243c5c47` | [MIT](https://github.com/chicogong/codex-chatgpt-web-orchestrator/blob/0781dc2f18d853b77c546d5d59c6bed1243c5c47/LICENSE) | Useful policy/receipt donor; pure policy functions and validators, not a bundled working browser backend |
| [jangtrinh/antigravity-codex-chatgpt-web](https://github.com/jangtrinh/antigravity-codex-chatgpt-web) | `09452a810230a2bc7e2ca6df3c754fd6671f8e52` | [MIT](https://github.com/jangtrinh/antigravity-codex-chatgpt-web/blob/09452a810230a2bc7e2ca6df3c754fd6671f8e52/LICENSE) | Do not adopt: coarse ephemeral Codex text wrapper plus brittle patches to an installed upstream compiled bundle |
| [daepo666/codex-webGPT-connect_drdaepopo](https://github.com/daepo666/codex-webGPT-connect_drdaepopo) | `1e84d39b2d3421fbf9b9f7ff4c61ddb2e78e8693` | No license identified in reviewed metadata/tree | Different workflow: Codex drives browser actions and separate CTF fan-out; not a general ChatGPT model transport; no copying without permission |
| [daepo666/codex-webgpt-connect](https://github.com/daepo666/codex-webgpt-connect) | No commit; repository empty at inspection | No license identified | No implementation to adopt |

## Why the upstream transport is the best fit

The upstream contains a real local Responses service and native-tool broker. Its broker forwards supported escalation/approval fields to the owning Codex harness and rejects unsupported fields instead of inventing a second local approval executor: [native broker](https://github.com/miuuyy/codex-chatgpt-web/blob/0b6087c5307c7a58adda9c420358f3f70d2035f5/src/adapters/chatgpt-web/mcp-server.ts#L621-L687).

The adapter also has source paths for registry checks, duplicate tool-result rejection, retained conversations, compaction, and ambiguous-send handling: [ChatGPT adapter](https://github.com/miuuyy/codex-chatgpt-web/blob/0b6087c5307c7a58adda9c420358f3f70d2035f5/src/adapters/chatgpt-web/index.ts#L326-L340). These are source advantages, not proof they work on this workstation/account today.

Linux browser path is configurable and the service enforces loopback binding: [configuration](https://github.com/miuuyy/codex-chatgpt-web/blob/0b6087c5307c7a58adda9c420358f3f70d2035f5/src/config.ts#L369-L379). Bun and browser/runtime dependencies must be reconciled with the chosen release. Do not treat a platform default Chrome path as proof that the executable exists locally.

Important security limit: inspected ordinary Responses endpoints do not require a bearer secret; admin routes use a control token, and source rejects non-GET Origin-bearing requests. Loopback and Origin checks do not isolate the bridge from all local processes. The client must not claim that an authenticated frontend/proxy secures a still-directly-reachable unauthenticated bridge. Determine an appropriate per-user/process/network containment boundary before activation, and document the residual local-peer trust model. [Server routes and controls](https://github.com/miuuyy/codex-chatgpt-web/blob/0b6087c5307c7a58adda9c420358f3f70d2035f5/src/server.ts#L966-L1015).

## Why not the smaller replay bridge

The a252 implementation opens a conversation for each request, renders the supplied transcript/tool catalog, asks for one action, validates it, and may run output-repair prompts. This is a different model/transport strategy, not a drop-in addition to upstream native conversation ownership: [service](https://github.com/a252937166/codex-chatgpt-web-bridge/blob/85cc3d01c99bc49be6baf039db556e82bbbe33d2/src/codex_web_bridge/bridge/service.py#L42-L102), [browser backend](https://github.com/a252937166/codex-chatgpt-web-bridge/blob/85cc3d01c99bc49be6baf039db556e82bbbe33d2/src/codex_web_bridge/backends/playwright_chatgpt.py#L31-L88).

Its SSE envelopes begin after backend completion rather than providing real token deltas: [SSE construction](https://github.com/a252937166/codex-chatgpt-web-bridge/blob/85cc3d01c99bc49be6baf039db556e82bbbe33d2/src/codex_web_bridge/responses/sse.py#L9-L45). Image/audio references are not actual uploads, and source has non-strict model-selection paths. The explicit local API authentication is useful reference material, but not enough to justify replacing the richer upstream session/tool machinery. Mock/default-development settings must never be represented as real provider connectivity.

## Bounded policy donor

The chicogong CLI plans from observation/capability input files; it explicitly does not open a browser or submit a prompt: [CLI](https://github.com/chicogong/codex-chatgpt-web-orchestrator/blob/0781dc2f18d853b77c546d5d59c6bed1243c5c47/scripts/orchestrator_cli.py#L1-L79).

Useful isolated concepts for the native Ironbark core:

- Prompt fingerprints and duplicate-send prevention.
- Verified route/model evidence before submission.
- Reconciliation of uncertain submission before considering a retry.
- Terminal completion plus actual output/artifact capture.
- Receipt commitments linking exact approved scope and inputs to results.

Source: [policy functions](https://github.com/chicogong/codex-chatgpt-web-orchestrator/blob/0781dc2f18d853b77c546d5d59c6bed1243c5c47/scripts/orchestrator_policy.py#L86-L163), [receipt validation](https://github.com/chicogong/codex-chatgpt-web-orchestrator/blob/0781dc2f18d853b77c546d5d59c6bed1243c5c47/scripts/delegation_receipt.py#L57-L277).

Translate only the appropriate semantics into the existing versioned job/approval/event model. Do not add a second scheduler or import its backend-presence heuristics as capability proof. Adapt its narrow mode/receipt families deliberately; do not force all Ironbark providers/harnesses into those labels. Hashes establish integrity commitments, not signatures or truthful observations. Public/private sidecar separation still requires filesystem/access enforcement. Retain applicable MIT attribution if code is ported.

## Rejected wrapper and unrelated snapshots

The Antigravity wrapper starts a fresh `codex exec --ephemeral`, buffers stdout, and returns cleaned text; it can treat nonzero exit with stdout as a usable result and lacks the durable native run/approval/cancellation contract needed here: [wrapper](https://github.com/jangtrinh/antigravity-codex-chatgpt-web/blob/09452a810230a2bc7e2ca6df3c754fd6671f8e52/src/mcp-server.js#L49-L113). Do not use an unpinned model alias as proof of the actual selected model.

Its image patcher targets generated/minified bundles, treats broad image observations as completion, and kills matching processes: [patcher](https://github.com/jangtrinh/antigravity-codex-chatgpt-web/blob/09452a810230a2bc7e2ca6df3c754fd6671f8e52/scripts/patch-bridge.js#L93-L159). No code delta from that patcher is recommended.

The daepo snapshot uses Codex as a browser-agent decision model, the reverse of using ChatGPT Web as a Codex model backend: [decision model](https://github.com/daepo666/codex-webGPT-connect_drdaepopo/blob/1e84d39b2d3421fbf9b9f7ff4c61ddb2e78e8693/codex-web-agent/codex_model.py#L20-L69). Its specialized CTF fan-out is not evidence of general-provider/session/tool support. No license was established for copying it.

## Local integration and activation gate

Use one canonical bridge with one dedicated authenticated ChatGPT browser profile and one genuine native Codex session owner per run. The recommended primary route is a supervised Codex app-server worker. The existing local OMP-to-native-Codex adapter is a separate reuse candidate requiring joint qualification; direct generic OMP interoperability is not an upstream feature to assume. Route other API providers normally; WebGPT is not a way to make an unsupported provider compatible with a harness.

Full tool mode also requires ChatGPT Developer Mode, the named `Codex Native2` connector, account/workspace tool permissions, and OpenAI's outbound secure MCP tunnel. The documented Authentication None / Allow all actions setup is a consequential account/security change requiring explicit interactive owner consent; none was performed. An outbound tunnel is not an inbound port or remote runner, but the connection is not wholly local. [Pinned setup requirements](https://github.com/miuuyy/codex-chatgpt-web/blob/0b6087c5307c7a58adda9c420358f3f70d2035f5/README.md#L88-L125).

Upstream expects Codex's built-in `openai` provider identity, manages route/hook configuration, and requires genuine native turn/environment authority. Use an isolated managed configuration only after proving its home/rollout/auth/hook behavior; do not rewrite the user's global custom-provider settings. [Provider restriction](https://github.com/miuuyy/codex-chatgpt-web/blob/0b6087c5307c7a58adda9c420358f3f70d2035f5/src/codex-integration-route.ts#L177-L186). Keep connector autoapproval disabled, qualify targeted cancellation across HTTP/browser/native hooks, and avoid competing launcher/process ownership.

The inspected package version is 6.1.8, but main contains changes after release-labeled commits. Pin the exact commit/release build, not merely the version string. Upstream composer predictions default enabled; the recommended managed profile disables them unless the owner opts into additional account traffic. [Defaults](https://github.com/miuuyy/codex-chatgpt-web/blob/0b6087c5307c7a58adda9c420358f3f70d2035f5/src/config.ts#L238-L265).

Observed local limitations:

- `127.0.0.1:17841` did not accept a connection during this session.
- The local OMP adapter pins SDK 18.6.1 while installed OMP reports 18.8.9.
- Local adapter documentation cites bridge 6.1.5 and Codex 0.160.0; inspected upstream package is 6.1.8.
- Existing generated wire types and native authority assumptions require coordinated qualification, not blind dependency upgrades.

Activation requires real text, tools, failure propagation, denied mutation, image input, cancellation, session-isolation, and native-history cases on the selected versions, plus explicit human-controlled login/provider consent and review of applicable terms. Do not change account or approval settings, invent provider turns, or execute a patcher simply to produce a green connection badge.

This is a **recommended source selection**, not a deployed integration or live compatibility claim.
