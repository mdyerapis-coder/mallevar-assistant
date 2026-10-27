# MALLEVAR Source Inventory

**Date:** 18 October 2026  
**Audit Host:** CachyOS laptop (`mason@100.98.24.101`)  
**Audit Tool:** OMP Autonomous Agent

---

## 1. Executive Summary

This document provides a comprehensive inventory of the Mallevar development work discovered during the Termux two-host synchronisation completed at `20261018T005514`.

### 1.1 Discovered Repositories

| Repository | Location | Branch | Status | Changes |
|------------|----------|--------|--------|---------|
| **ironbark-assistant** | `/home/mason/Projects/ironbark-assistant/.worktrees/build` | `feat/workstation-assistant` | Dirty | 68 modified, 44 untracked |
| **ForgeOS** | `/home/mason/Projects/ForgeOS` | `work/ironbark-bd6df89` | Dirty | 2 modified, new evidence dir |
| **forge-router-lab** | `/home/mason/Work/forge-source-audit/forge-router-lab` | `safety/pre-consolidation-20261001` | Clean | Published to GitHub |
| **agent-forge** | `/home/mason/Work/forge-source-audit/agent-forge` | `feature-ironbark-roots` | Untracked | Nested worktree |

### 1.2 Repository Health Metrics

```
┌─────────────────────────────────────────────────────────────────┐
│ Repository              │ Branch    │ Commits Ahead │ Files     │
├─────────────────────────────────────────────────────────────────┤
│ ironbark-assistant      │ main      │ 6             │ 68/44     │
│ ForgeOS                 │ origin    │ Up to date    │ 2         │
│ forge-router-lab        │ origin    │ Up to date    │ 0         │
│ agent-forge             │ local     │ N/A           │ New       │
└─────────────────────────────────────────────────────────────────┘
```

---

## 2. ironbark-assistant — Assistant Implementation

### 2.1 Repository Overview

**Location:** `/home/mason/Projects/ironbark-assistant/.worktrees/build`  
**Current HEAD:** `835b8caec405b51f0c026e99ee527e2692571299`  
**Branch:** `feat/workstation-assistant`  
**Upstream:** `origin/work/mallevar-laptop-20261018T005516-835b8caec405`

### 2.2 File Structure

```
ironbark-assistant/
├── crates/
│   ├── ironbark-cli/          # CLI entry point
│   └── ironbark-core/          # Core Rust implementation
│       ├── src/
│       │   ├── contracts.rs    # Protocol contracts
│       │   ├── automation/     # Target automation
│       │   ├── connections/    # WebGPT integration
│       │   ├── execution/      # Worker execution
│       │   ├── judgments/      # Qualification logic
│       │   ├── knowledge/      # Knowledge sources
│       │   ├── policy/         # Policy management
│       │   ├── process/        # Process supervision
│       │   └── store/          # Persistence layer
│       └── tests/              # Unit and integration tests
├── packages/
│   ├── adapters/               # TypeScript adapters
│   │   ├── src/
│   │   │   ├── harness/        # Harness implementations
│   │   │   └── jev/            # JEV evaluation
│   │   └── test/               # Adapter tests
│   └── contracts/              # Contract definitions
├── docs/
│   └── runtime/                # Runtime documentation
└── qa/                         # Quality assurance artifacts
```

### 2.3 Source Changes (68 Files)

| Category | Files | Description |
|----------|-------|-------------|
| **Core Contracts** | 12 | Protocol definitions, wire serialization |
| **Automation** | 8 | Target automation, approval boundaries |
| **Execution** | 10 | Worker lifecycle, policy enforcement |
| **Judgments** | 6 | Qualification metrics, evaluation reports |
| **Knowledge** | 8 | Source integration, chunk extraction |
| **Policy** | 6 | Service routing, file management |
| **Process** | 8 | Launcher, supervisor, sandbox |
| **Store** | 10 | Backup, migrations, retention |
| **Tests** | 16 | Unit tests, integration scenarios |
| **Documentation** | 4 | Runtime guides, specifications |
| **Configuration** | 4 | Build files, environment setup |

### 2.4 Untracked Files (44 Files)

| File | Purpose |
|------|---------|
| `c1-dataset-impl-report.md` | Dataset implementation report |
| `c1-dataset-red-report.md` | Dataset red team report |
| `crates/ironbark-core/src/contracts/wire.rs` | Wire contract generation |
| `crates/ironbark-core/src/execution/worker.rs` | Worker execution |
| `crates/ironbark-core/src/store/preflight.rs` | Preflight validation |
| `crates/ironbark-core/src/store/schema.rs` | Schema definitions |
| `crates/ironbark-core/tests/*.rs` | Storage and wire tests |
| `docs/superpowers/plans/2026-10-15-all65-*.md` | All-65 planning documents |
| `packages/adapters/src/harness/worker.ts` | Worker protocol |
| `packages/adapters/test/*.test.ts` | Adapter tests |

---

## 3. ForgeOS — Installer Validation

### 3.1 Repository Overview

**Location:** `/home/mason/Projects/ForgeOS`  
**Current HEAD:** `b3845c01202a8ddd0d84c344f55361aeb163c65a`  
**Branch:** `work/ironbark-bd6df89`  
**Status:** Up to date with origin

### 3.2 Validation Scripts

| Script | Purpose | Size | Status |
|--------|---------|------|--------|
| `install-driver.py` | Driver installation | 5,882 bytes | Modified |
| `install_flow.py` | Flow configuration | 10,190 bytes | Modified |
| `scan.py` | System scanning | 9,780 bytes | Stable |
| `verify-trace-boundary.py` | Trace verification | 2,082 bytes | Stable |

### 3.3 Evidence Directory

```
evidence/bd6df894277f6012a292a931d0ac3e9460decac0/
└── [limine-boot-data]/
```

---

## 4. forge-router-lab — Router Donor Audit

### 4.1 Repository Overview

**Location:** `/home/mason/Work/forge-source-audit/forge-router-lab`  
**Current HEAD:** `a3a3801`  
**Branch:** `safety/pre-consolidation-20261001`  
**Status:** Clean, published to GitHub

### 4.2 Documentation

| Document | Lines | Purpose |
|----------|-------|---------|
| `README.md` | 284 | Project overview |
| `PROMPT-deb-audit-extend.md` | 354 | Debian audit prompts |
| `PROMPT-deb-package.md` | 308 | Package management |
| `RUNBOOK.md` | 96 | Operational procedures |

---

## 5. Credential and Security Status

### 5.1 GitHub Credentials

- **Account:** `mdyerapis-coder`
- **SSH Key:** `id_ed25519_github` (411 bytes)
- **Helper:** GitHub CLI (`gh auth git-credential`)
- **Configuration:** `~/.git-credentials`

### 5.2 Bitwarden Integration

- **CLI Location:** `/home/mason/.local/share/mise/shims/bw`
- **Status:** Locked
- **Last Sync:** `2026-10-14T12:11:38.011Z`
- **User:** `dyer.mason1994@gmail.com`

### 5.3 SSH Key Inventory

| Key | Purpose | Status |
|-----|---------|--------|
| `id_ed25519_github` | GitHub authentication | Active |
| `assistant_vps` | VPS access | Active |
| `flow-workstation-sshd.conf` | Flow workspace | Active |
| `runpod_ironbark_ed25519` | Runpod integration | Active |
| `masons-ground` | Host identity | Active |

---

## 6. Test Results Summary

### 6.1 TypeScript Tests (Bun)

- **Total Tests:** 355
- **Passed:** 334
- **Skipped:** 20
- **Failed:** 1
- **Execution Time:** 101.37s

**Failure:** `valid_raw_record_retains_identity_mapping_and_unknown_qualification`
- **Issue:** `latency_ms` type validation
- **Root Cause:** `formatWireU64` produces string instead of number

### 6.2 Rust Tests

- **Total Tests:** 40
- **Passed:** 40
- **Failed:** 0
- **Warnings:** 24
- **Execution Time:** 1.09s

**Key Areas:**
- Qualification gates and enablement
- Worker race conditions
- Preflight validation
- Knowledge extraction

---

## 7. Recommendations

### 7.1 Immediate Actions

1. **Fix TypeScript Test:** Update `latency_ms` type handling in JEV client
2. **Stage and Commit:** Review and commit the 68 modified files
3. **Untracked Files:** Add the 44 untracked files to version control

### 7.2 Medium-Term Goals

1. **CI Integration:** Configure automated testing and deployment
2. **Documentation:** Publish planning documents and runbooks
3. **Version Alignment:** Coordinate releases across repositories

### 7.3 Long-Term Objectives

1. **Production Deployment:** Migrate validated work to production
2. **Monitoring:** Implement health monitoring and alerting
3. **Continuous Improvement:** Establish feedback loops for ongoing enhancement

---

## 8. Appendix

### 8.1 Git Commands Reference

```bash
# Check repository status
git status --short

# View commit history
git log --oneline --graph -10

# Review changes
git diff HEAD

# Stage changes
git add -p

# Commit with message
git commit -m "Mallevar convergence: stage validated changes"

# Push to remote
git push origin feat/workstation-assistant
```

### 8.2 Directory Tree

```
/home/mason/Projects/
├── ironbark-assistant/
│   └── .worktrees/
│       └── build/ (primary worktree)
├── ForgeOS/ (installer)
└── ironbark-webgpt-sidecar/ (sidecar)

/home/mason/Work/
└── forge-source-audit/
    ├── agent-forge/
    └── forge-router-lab/
```

---

**Generated by:** OMP Autonomous Agent  
**Document Version:** 1.0  
**Last Updated:** 18 October 2026
