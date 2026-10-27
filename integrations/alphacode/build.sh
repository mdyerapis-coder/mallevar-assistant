#!/usr/bin/env bash
# AlphaCode source build recipe for Ironbark.
#
# Fetches the exact pinned upstream commit into .sources/alphacode/, applies the
# recorded patch series from patches/ in order, builds the Ironbark-managed
# binary and records the resulting build-artifact digest. It never executes the
# compiled AlphaCode (see source.json "must_not_run_compiled_binary").
#
# Provenance is fail-closed: every input is pinned by digest in source.json and
# any unpinned input aborts the build instead of being applied.
set -euo pipefail

integration_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$integration_dir/../.." && pwd)"
source_json="$integration_dir/source.json"
patches_dir="$integration_dir/patches"
source_dir="${ALPHACODE_SOURCE_DIR:-$repo_root/.sources/alphacode}"
artifact_manifest="${ALPHACODE_ARTIFACT_MANIFEST:-$integration_dir/build-artifact.json}"
target_dir="${CARGO_TARGET_DIR:-$repo_root/qa/private/alphacode-target}"
toolchain="1.94.1"

[[ -f "$source_json" ]] || { echo "Missing $source_json" >&2; exit 2; }
command -v python3 >/dev/null || { echo 'python3 is required to read source.json' >&2; exit 2; }

# --- record -----------------------------------------------------------------
mapfile -t meta < <(python3 - "$source_json" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
print(doc["repository"])
print(doc["commit"])
print(doc["git_tree"])
print(doc["build_recipe"]["rust"])
print(doc["source_recipe_digest"])
for entry in doc["patch_series"]:
    print("SERIES\t%s\t%s" % (entry["path"], entry["sha256"]))
for entry in doc["licenses"]:
    print("LICENSE\t%s\t%s" % (entry["path"], entry["sha256"]))
PY
)
upstream="${meta[0]}"
commit="${meta[1]}"
tree="${meta[2]}"
recorded_toolchain="${meta[3]}"
recipe_digest="${meta[4]}"
series=()
licenses=()
for line in "${meta[@]:5}"; do
  case "$line" in
    SERIES*)    series+=("${line#SERIES$'\t'}") ;;
    LICENSE*)   licenses+=("${line#LICENSE$'\t'}") ;;
    *) echo "Unparsable source.json record: $line" >&2; exit 2 ;;
  esac
done
[[ "$recorded_toolchain" == "$toolchain" ]] || { echo "Toolchain drift: recipe expects $toolchain, source.json records $recorded_toolchain" >&2; exit 2; }
[[ "${#series[@]}" -gt 0 ]] || { echo 'source.json records an empty patch series' >&2; exit 2; }

echo "AlphaCode ${commit} from ${upstream}"

# --- fetch the exact commit into .sources/alphacode --------------------------
if [[ ! -d "$source_dir/.git" ]]; then
  mkdir -p "$(dirname "$source_dir")"
  git init -q "$source_dir"
  git -C "$source_dir" remote add origin "$upstream"
fi
git -C "$source_dir" remote set-url origin "$upstream"
git -C "$source_dir" fetch -q --depth 1 origin "$commit"
git -C "$source_dir" -c advice.detachedHead=false checkout -q --force --detach "$commit"
# Discard any previously applied series so the checkout is exactly the pinned
# revision before this run's patches are applied.
git -C "$source_dir" reset -q --hard "$commit"
git -C "$source_dir" clean -q -ffd

actual_commit="$(git -C "$source_dir" rev-parse HEAD)"
actual_tree="$(git -C "$source_dir" rev-parse 'HEAD^{tree}')"
[[ "$actual_commit" == "$commit" ]] || { echo "Fetched commit $actual_commit is not the pinned $commit" >&2; exit 2; }
[[ "$actual_tree" == "$tree" ]] || { echo "Fetched tree $actual_tree is not the recorded $tree" >&2; exit 2; }

# --- verify the upstream notice files the NOTICE reproduces ------------------
for entry in "${licenses[@]}"; do
  path="${entry%%$'\t'*}"
  want="${entry##*$'\t'}"
  got="$(git -C "$source_dir" show "$commit:$path" | sha256sum | cut -d' ' -f1)"
  [[ "$got" == "$want" ]] || { echo "Licence digest mismatch for $path: $got != $want" >&2; exit 2; }
done

# --- reject unpinned patch files ---------------------------------------------
for candidate in "$patches_dir"/*.patch; do
  [[ -e "$candidate" ]] || continue
  rel="patches/$(basename "$candidate")"
  found=0
  for entry in "${series[@]}"; do
    [[ "${entry%%$'\t'*}" == "$rel" ]] && found=1
  done
  [[ "$found" == 1 ]] || { echo "Unpinned patch $rel is present but absent from source.json patch_series; record it (with its sha256) or remove it" >&2; exit 2; }
done

# --- apply the recorded series in order --------------------------------------
for entry in "${series[@]}"; do
  rel="${entry%%$'\t'*}"
  want="${entry##*$'\t'}"
  file="$integration_dir/$rel"
  [[ -f "$file" ]] || { echo "Missing recorded patch $rel" >&2; exit 2; }
  got="$(sha256sum "$file" | cut -d' ' -f1)"
  [[ "$got" == "$want" ]] || { echo "Patch digest mismatch for $rel: $got != $want" >&2; exit 2; }
  git -C "$source_dir" apply --check "$file"
  git -C "$source_dir" apply "$file"
  echo "Applied $rel"
done

# --- verify the patched per-file digests -------------------------------------
python3 - "$source_json" "$source_dir" <<'PY'
import hashlib, json, os, sys
doc = json.load(open(sys.argv[1]))
source_dir = sys.argv[2]
for entry in doc["files"]:
    path = os.path.join(source_dir, entry["path"])
    data = open(path, "rb").read()
    got = hashlib.sha256(data).hexdigest()
    want = entry["patched_sha256"]
    if want is not None and got != want:
        raise SystemExit("Patched digest mismatch for %s: %s != %s" % (entry["path"], got, want))
PY

# --- recompute the recipe digest and build -----------------------------------
series_files=()
for entry in "${series[@]}"; do
  series_files+=("$integration_dir/${entry%%$'\t'*}")
done
built_digest="$( { printf '%s' "$commit"; cat "${series_files[@]}" "$integration_dir/build.sh" "$source_dir/Cargo.lock" "$source_dir/rust-toolchain.toml"; } | sha256sum | cut -d' ' -f1 )"
[[ "$built_digest" == "$recipe_digest" ]] || { echo "Recomputed recipe digest $built_digest does not match source.json $recipe_digest; regenerate source.json" >&2; exit 2; }

export IRONBARK_ALPHACODE_BUILD_DIGEST="$recipe_digest"
export ALPHACODE_NO_TELEMETRY=1
export CARGO_TARGET_DIR="$target_dir"
# A preinstalled pinned toolchain is required. Nothing here installs a toolchain,
# activates an extension, or executes the compiled binary.
( cd "$source_dir" && cargo "+$toolchain" build --locked --release --no-default-features --bin alphacode )

binary="$target_dir/release/alphacode"
[[ -f "$binary" ]] || { echo "Build produced no binary at $binary" >&2; exit 2; }
binary_sha256="$(sha256sum "$binary" | cut -d' ' -f1)"

# --- record the build artifact digest ---------------------------------------
python3 - "$artifact_manifest" "$commit" "$recipe_digest" "$binary_sha256" "$toolchain" <<'PY'
import datetime, json, sys
manifest, commit, recipe, binary, toolchain = sys.argv[1:6]
doc = {
    "schema_version": 1,
    "commit": commit,
    "source_recipe_digest": recipe,
    "rust_toolchain": toolchain,
    "binary_sha256": binary,
    "built_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "executed": False,
}
with open(manifest, "w") as handle:
    json.dump(doc, handle, indent=2, sort_keys=True)
    handle.write("\n")
print("source_recipe_digest=%s" % recipe)
print("binary_sha256=%s" % binary)
print("wrote %s" % manifest)
PY
