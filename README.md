# Mallevar Assistant (formerly Ironbark Assistant)

Mallevar is the current product identity. This repository retains the original Rust crate, executable, package and data-path names for compatibility until their consumers and migration tests are verified.

A standalone, local-first workstation assistant: one Rust core owns durable state, policy and process boundaries; Bun adapter workers carry provider and JEV transports. The C4 scheduler and live provider launch are **not yet qualified**; no Roots runtime implementation or release is implied by the name change. Adapters never become a second scheduler.

See [Mallevar Assistant ↔ Roots integration boundary](docs/mallevar-roots-integration.md) for the proposed single-authority design and required acceptance tests. The OS convergence work remains in [the ForgeOS integration repository](https://github.com/mdyerapis-coder/ForgeOS/tree/work/ironbark-convergence).

## Components

| Piece | What it is |
| --- | --- |
| `crates/ironbark-core` | Native daemon: SQLite store, authenticated local Unix IPC, policy engine, service dispatcher, worker/network containment boundary. Also builds `namespace_bridge` (the private network bridge staged inside worker namespaces). |
| `crates/ironbark-cli` | Operator CLI. Every subcommand prints exactly one JSON DTO on stdout; `command '<CoreCommand JSON>'` is the generic wire form. `backup restore` runs offline without a daemon. |
| `packages/adapters` | Bun worker: provider registry + OMP/local/custom provider adapters, JEV judgment client. Transport only — no core database access or policy writes. |
| `packages/contracts` | Rust-generated protocol-1 TypeScript contracts (`generated.ts`, `schema.json`). |
| `integrations/alphacode` | Pinned upstream AlphaCode build, patch and notices for the H7 permission-boundary extension. |
| `qa/` | Rust integration tests and Bun smokes (reconnect, worker containment, JEV). |
| `docs/runtime/` | Verified behavior: core, permissions, containment, providers. |
| `docs/design/` | Approved design bundle and implementation plans. |

## Build and run

Toolchain is pinned in `rust-toolchain.toml` (Rust 1.98.1, no global rustup
default required) and `package.json` (Bun 1.4.3).

```sh
cargo build -p ironbark-core -p ironbark-cli
./target/debug/ironbark-core          # prints CORE_READY protocol=1
./target/debug/ironbark-cli doctor --json
./target/debug/ironbark-cli project create example --root /absolute/project/path
./target/debug/ironbark-cli status
./target/debug/ironbark-cli shutdown
```

Check the same way the repository is verified:

```sh
bun qa/qualification/run-gates.ts
bun qa/qualification/release.ts --manifest packaging/manifest.json
```

The runner/checker require version-2 observed receipts. Each gate binds the
current commit/tree, selected tracked and untracked product bytes/modes,
required contracts/locks, toolchain executables, exact argv/cwd and the explicit
gate environment. Source drift during or after a gate blocks release, even if
the command exited zero. Stdout and stderr retain separate 64 KiB byte tails
with original/dropped byte counts; binary tails are losslessly labeled base64.

Use a qualification checkout without private `.env`, `.env.local`, or
development/test/production dotenv variants: preflight refuses them without
reading or copying their contents. Automatic Bun/Cargo configuration is part of
the source inventory. Undeclared parent environment variables are not delivered
to gates; supported selectors such as `IRONBARK_CORE_BIN`, `IRONBARK_CLI_BIN` and
compiler wrappers are delivered and hash-bound. Live harness/provider
qualification is separate from the default isolated executable tests.

Worker execution needs `bubblewrap` (namespace isolation) and `bun`
(adapter runtime); the containment smoke exercises real process/namespace
termination evidence.

## Status

The core, policy, C5 objects/backup/restore, P2 conversations, the
network/process containment boundary and the provider/JEV transport contracts
are implemented and verified on this workstation. Run scheduling and live
provider launch (C4), the live provider/JEV/AlphaCode qualification smokes,
and the desktop release are not yet qualified — see `docs/runtime/` for the
exact boundary of what is verified and what is not.

Earlier verification (2026-10-15 UTC) reported the eight required gates
**blocked, 7/8 passed**, with installed-Hermes discovery timeouts. Those old
receipts do not qualify the current source. The all-65 work replaces ordinary
harness discovery with isolated executables and adds exact-source v2 evidence;
its complete joined release gate has not yet run. Focused JEV regressions
(44 tests), native qualification tests (13), the earlier workspace Cargo gate
and TypeScript checking passed.
The earlier synthetic JEV diagnostic run did not qualify a domain; repeated inputs and inverted labels
invalidate it as independent holdout evidence. See `docs/runtime/providers.md`.
Native resumable interruption and the interactive desktop walkthrough remain
incomplete; no all-five completion or main-merge claim is made.

## Packaging

- **Arch (AUR):** `packaging/aur/PKGBUILD` installs the core, CLI, desktop
  binary and namespace bridge. WebKitGTK is a runtime dependency and supplies
  JavaScriptCore on Arch; the unenabled tray feature does not require appindicator.
- **RPM:** `packaging/rpm/ironbark-assistant.spec`. Both recipes honor
  `CARGO_TARGET_DIR` when installing release binaries.

Current local candidates were built with actual `makepkg` and `rpmbuild -bb`.
Extracted core/CLI payloads from both packages passed isolated status,
blocked-qualification persistence, restart/readback and shutdown smokes;
both desktop files validate. The RPM was built and exercised on Arch with
RPM dependency-database lookup skipped, not qualified on Fedora.

The existing GitHub `v0.1.0` RPM is historical. These candidates are not uploaded,
signed or a replacement for that tag. GitHub authentication is invalid and the
default AUR SSH identity is rejected. A local package build does not qualify the
GUI, native run control or judgment domains.

## License

MIT — see [LICENSE](LICENSE).
