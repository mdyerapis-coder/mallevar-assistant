#!/usr/bin/env bash
# Ironbark Assistant — user-local installation (plan D5).
#
# Builds the release binaries from this source tree and installs them under a
# prefix, together with the desktop entry, icon and licences. It records a
# manifest of exactly what it installed (path, size, sha256, source revision)
# so a later `--check` can answer "is the installed copy current?" without
# guesswork.
#
# Guarantees:
#   * Nothing outside the prefix is touched; no root is required.
#   * Every file it replaces is backed up under <prefix>/share/ironbark-assistant/backups/<timestamp>/.
#   * It never installs a component it did not build (no placeholder binaries).
#   * `--dry-run` prints the exact plan and changes nothing.
#
# Usage:
#   packaging/local-install.sh [--prefix DIR] [--dry-run] [--check] [--with-desktop] [--uninstall]
#
# Defaults: --prefix ~/.local
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PREFIX="${HOME}/.local"
DRY_RUN=0
CHECK=0
WITH_DESKTOP=0
UNINSTALL=0

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
        --prefix=*) PREFIX="${1#--prefix=}"; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        --check) CHECK=1; shift ;;
        --with-desktop) WITH_DESKTOP=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

say() { printf '%s\n' "$*"; }
run() {
    if [ "${DRY_RUN}" = 1 ]; then printf '  would run: %s\n' "$*"; else "$@"; fi
}

STATE_DIR="${PREFIX}/share/ironbark-assistant"
MANIFEST="${STATE_DIR}/installed-manifest.json"
BIN_DIR="${PREFIX}/bin"
APPS_DIR="${PREFIX}/share/applications"
ICON_DIR="${PREFIX}/share/icons/hicolor/scalable/apps"
LICENSE_DIR="${PREFIX}/share/licenses/ironbark-assistant"

revision() {
    git -C "${REPO_ROOT}" rev-parse HEAD 2>/dev/null || echo "unknown"
}

# The revision alone cannot tell whether the installed binaries contain
# uncommitted work, so the manifest also records a digest of the worktree
# state (tracked modifications plus untracked paths) and `--check` compares it.
worktree_digest() {
    {
        git -C "${REPO_ROOT}" status --porcelain=v1 2>/dev/null || true
        git -C "${REPO_ROOT}" diff HEAD 2>/dev/null || true
    } | sha256sum | cut -d' ' -f1
}

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }

# ── --check: compare the installed manifest against this tree ────────────────
if [ "${CHECK}" = 1 ]; then
    if [ ! -f "${MANIFEST}" ]; then
        if [ -x "${BIN_DIR}/ironbark-core" ] || [ -x "${BIN_DIR}/ironbark-cli" ]; then
            say "Binaries exist under ${BIN_DIR} but there is no install manifest at ${MANIFEST}:"
            ls -la "${BIN_DIR}/ironbark-core" "${BIN_DIR}/ironbark-cli" "${BIN_DIR}/ironbark-desktop" 2>/dev/null | sed 's/^/  /' || true
            say "That install predates this installer and cannot be verified against the tree."
            say "Re-run the installer (without --check) to make the install verifiable."
            exit 1
        fi
        say "nothing is installed under ${PREFIX}"
        exit 1
    fi
    installed_rev="$(sed -n 's/.*"source_revision": *"\([^"]*\)".*/\1/p' "${MANIFEST}" | head -1)"
    installed_tree="$(sed -n 's/.*"source_worktree_digest": *"\([^"]*\)".*/\1/p' "${MANIFEST}" | head -1)"
    say "installed revision: ${installed_rev}"
    say "tree revision:      $(revision)"
    stale=0
    while read -r bin; do
        [ -f "${bin}" ] || { say "MISSING: ${bin}"; stale=1; continue; }
        say "installed: $(sha256_of "${bin}")  $(basename "${bin}")"
    done < <(printf '%s\n' "${BIN_DIR}/ironbark-core" "${BIN_DIR}/ironbark-cli")
    if [ "${installed_rev}" != "$(revision)" ]; then
        say "STALE: the installed binaries were built from a different revision"
        stale=1
    fi
    if [ "${installed_tree}" != "$(worktree_digest)" ]; then
        say "STALE: the source worktree has changed since this install (uncommitted work is not in the installed binaries)"
        stale=1
    fi
    [ "${stale}" = 0 ] && say "Installed copies match the current tree." || say "Reinstall required: re-run without --check."
    exit 0
fi

# ── --uninstall: remove exactly what the manifest records ───────────────────
if [ "${UNINSTALL}" = 1 ]; then
    [ -f "${MANIFEST}" ] || { say "no manifest at ${MANIFEST}; refusing to guess what to remove"; exit 1; }
    say "Removing files recorded in ${MANIFEST}"
    python3 - "${MANIFEST}" <<'PY' | while read -r path; do
import json, sys
with open(sys.argv[1]) as handle:
    manifest = json.load(handle)
for entry in manifest.get("files", []):
    print(entry["path"])
PY
        case "${path}" in
            "${PREFIX}"/*) run rm -f "${path}" ;;
            *) say "  refusing to remove a path outside the prefix: ${path}" ;;
        esac
    done
    run rm -f "${MANIFEST}"
    say "Uninstalled. Runtime state under \$XDG_DATA_HOME/ironbark-assistant is left untouched."
    exit 0
fi

# ── Build ───────────────────────────────────────────────────────────────────
say "Source tree: ${REPO_ROOT}"
say "Revision:    $(revision)"
say "Prefix:      ${PREFIX}"
say "Building release binaries (core, cli)…"
run cargo build --manifest-path "${REPO_ROOT}/Cargo.toml" --release --bin ironbark-core --bin ironbark-cli

CORE_BIN="${REPO_ROOT}/target/release/ironbark-core"
CLI_BIN="${REPO_ROOT}/target/release/ironbark-cli"
if [ "${DRY_RUN}" = 0 ]; then
    for bin in "${CORE_BIN}" "${CLI_BIN}"; do
        [ -x "${bin}" ] || { echo "build did not produce ${bin}" >&2; exit 1; }
    done
fi

DESKTOP_BIN=""
if [ "${WITH_DESKTOP}" = 1 ]; then
    say "Building the desktop client (Tauri)…"
    run cargo build --manifest-path "${REPO_ROOT}/apps/desktop/src-tauri/Cargo.toml" --release
    DESKTOP_BIN="${REPO_ROOT}/apps/desktop/src-tauri/target/release/ironbark-desktop"
    if [ "${DRY_RUN}" = 0 ] && [ ! -x "${DESKTOP_BIN}" ]; then
        echo "desktop build did not produce ${DESKTOP_BIN}" >&2
        exit 1
    fi
fi

# ── Plan / install ──────────────────────────────────────────────────────────
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BACKUP_DIR="${STATE_DIR}/backups/${TIMESTAMP}"
installable=()
installable+=("${CORE_BIN}:${BIN_DIR}/ironbark-core:755")
installable+=("${CLI_BIN}:${BIN_DIR}/ironbark-cli:755")
[ -n "${DESKTOP_BIN}" ] && installable+=("${DESKTOP_BIN}:${BIN_DIR}/ironbark-desktop:755")
installable+=("${SCRIPT_DIR}/desktop/ironbark-assistant.desktop:${APPS_DIR}/ironbark-assistant.desktop:644")
installable+=("${SCRIPT_DIR}/desktop/ironbark-assistant.svg:${ICON_DIR}/ironbark-assistant.svg:644")
installable+=("${REPO_ROOT}/LICENSE:${LICENSE_DIR}/LICENSE:644")

say ""
say "Install plan (${#installable[@]} files):"
for entry in "${installable[@]}"; do
    src="${entry%%:*}"; rest="${entry#*:}"; dst="${rest%%:*}"
    if [ ! -f "${src}" ]; then
        echo "missing source file: ${src}" >&2
        exit 1
    fi
    marker=""
    [ -f "${dst}" ] && marker=" (replaces existing, backed up)"
    printf '  %s -> %s%s\n' "${src}" "${dst}" "${marker}"
done

if [ "${DRY_RUN}" = 1 ]; then
    say ""
    say "Dry run: nothing was installed."
    exit 0
fi

run mkdir -p "${BIN_DIR}" "${APPS_DIR}" "${ICON_DIR}" "${LICENSE_DIR}" "${STATE_DIR}"

files_json="[]"
for entry in "${installable[@]}"; do
    src="${entry%%:*}"; rest="${entry#*:}"; dst="${rest%%:*}"; mode="${rest##*:}"
    if [ -f "${dst}" ]; then
        run mkdir -p "${BACKUP_DIR}"
        run cp -p "${dst}" "${BACKUP_DIR}/$(basename "${dst}")"
    fi
    run install -Dm"${mode}" "${src}" "${dst}"
    digest="$(sha256_of "${dst}")"
    size="$(stat -c %s "${dst}")"
    files_json="$(python3 -c '
import json,sys
files = json.loads(sys.argv[1])
files.append({"path": sys.argv[2], "size": int(sys.argv[3]), "sha256": sys.argv[4]})
print(json.dumps(files))
' "${files_json}" "${dst}" "${size}" "${digest}")"
    say "installed ${dst}"
done

python3 -c '
import json,sys
manifest = {
    "schema_version": "1.0",
    "package_name": "ironbark-assistant",
    "source_revision": sys.argv[2],
    "source_worktree_digest": sys.argv[6],
    "prefix": sys.argv[3],
    "installed_at": sys.argv[4],
    "files": json.loads(sys.argv[1]),
}
with open(sys.argv[5], "w") as handle:
    json.dump(manifest, handle, indent=2, sort_keys=True)
    handle.write("\n")
' "${files_json}" "$(revision)" "${PREFIX}" "${TIMESTAMP}" "${MANIFEST}" "$(worktree_digest)"

say ""
say "Installed under ${PREFIX}. Manifest: ${MANIFEST}"
say "Replaced files (if any) were backed up under ${BACKUP_DIR}."
say "Run '${BASH_SOURCE[0]}' --check to compare the install against this tree,"
say "or '${BASH_SOURCE[0]}' --uninstall to remove exactly what was installed."
say ""
say "The core daemon binds a per-user Unix socket only; it must be started"
say "explicitly: ${BIN_DIR}/ironbark-core"
