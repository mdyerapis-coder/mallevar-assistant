# Builder check — Mallevar Assistant design

Date: 2026-10-12. Inline planning review, not an independent audit or production approval.

Reviewed the design as a builder who must plan implementation without additional conversation. Repository research was independently split across two read-only scouts; the final product-design review was performed inline.

## Design questions resolved

| Question that changes the build | Resolution |
| --- | --- |
| Is this a local desktop app, a server platform, or an OS rebuild? | Standalone Linux workstation app. No remote runners or hosted orchestration. Existing ForgeDeck code is donor material, not permission to modify/rebuild ForgeOS. |
| Is OMP the global product scheduler? | No. One deterministic local core owns jobs, state, permissions, and evidence. OMP remains the primary genuine coding executor. |
| Do all provider/harness combinations have to work? | No invented compatibility. Providers are extensible, with real protocol/capability adapters; combinations are explicitly qualified. |
| How much of each native harness survives? | Native sessions, tools, approvals, and failures remain real. Unsupported automation is not replaced with a screen-scraping success path. |
| Does changing a harness transfer its private session? | No. A new native session receives an explicit handoff package; original transcripts retain provenance. |
| Which WebGPT repository should be adopted? | miuuyy source is the recommended transport candidate, pinned to reviewed identity. chicogong supplies bounded policy/receipt concepts. No wholesale merge; other alternatives have incompatible/reduced contracts or missing license/implementation. |
| Can WebGPT be wired as a generic OpenAI-compatible endpoint? | No. Genuine Codex ownership/native metadata are required. The local OMP-native adapter is a separate joint-qualification candidate. |
| Is WebGPT full mode wholly local? | No. Local execution plus remote ChatGPT, an outbound MCP tunnel, and account connector setup. Consequential configuration/consent is deferred to explicit activation approval. |
| Is local JEV ready to decide assistant actions? | The endpoint is live but current generic task enum smoke matched only 1/4. Reuse is required; assistant domains must be qualified before automatic use. JEV remains advisory and cannot grant permissions. |
| What is the knowledge-base boundary? | Versioned sources, approved scoped memory, and transient context are distinct; retrieval/disclosure filtering happens before sending context. Citations retain revision identity. |
| How is parallel editing coordinated? | One writer per contained worktree; graph dependencies and resource limits; read-only reviews; one integration owner for shared changes. |
| Are worktrees a sandbox? | No. Native approvals and a qualified Linux containment mechanism are required for unattended mutation. The implementation plan chooses and proves one supported mechanism. |
| What happens after cancellation/crash? | Verify owned-process termination; preserve partial evidence; reconcile actual state before resuming. Never blindly replay uncertain mutation. |
| What visual direction is approved? | The restrained Ironbark conversation-first preview. Original artwork stays outside working text/code/control surfaces. |
| Does approval of the mockup authorize implementation? | No. Full written-design approval is the next gate; activation has further exact-scope consent gates. |

## Engineering calls left to implementation planning

File/module layout, protocol bindings and pin matrix, supported Linux isolation mechanism, parser/index/embedding implementation, native lifecycle integration, desktop packaging, performance targets tied to actual hardware, exact commands and tests. These choices must satisfy DESIGN.md; they cannot remove a promised capability or loosen approval/disclosure boundaries.

## Owner-controlled activation prerequisites

- Approval of the written product design before the detailed implementation plan.
- Artwork rights/attribution disposition before distributing originals.
- Provider/account terms and exact login/connector/tunnel/trusted-hook setup consent before WebGPT activation.
- Explicit owner choice if the shared File Steward model or service configuration must change.

No additional conversational question is required merely to write the implementation plan once this design is approved. The listed activation decisions are not assumed grants.

## Verification observed

- Native CLI help surfaces inspected for OMP, Codex, OpenCode, and Claude Code; binaries identified for Hermes as well.
- Interactive browser mockup exercised: navigation, citations, optional task panel, imagery, WebGPT provider selection, and local JEV Settings entry. Send remains disabled and no fake connection is claimed.
- Real local JEV health and four `/v1/decisions` responses preserved in `jev-smoke.json`.
- Bridge loopback probe failed to connect at port 17841; no live WebGPT model verified.
- A separate native `openai-codex/gpt-6-astra` invocation reviewed the screenshot and returned substantive feedback recommending the approved layout; recorded in `ASTRA-REVIEW.md`. This was not a top-level model switch or WebGPT activation.
- GitHub comparisons are pinned source review, not runtime verification.

Outcome: sufficient to prepare a detailed implementation plan after owner approval, with explicit qualification/activation gates. Not sufficient to claim a connected, safe, packaged, or complete product.
