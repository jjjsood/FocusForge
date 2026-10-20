#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
usage() { echo "usage: $0 INSTALLED_CLI [OUTPUT_DIRECTORY]" >&2; exit 2; }
[[ $# -ge 1 && $# -le 2 ]] || usage

cli=$1
case $cli in
  */*)
    [[ -f $cli && -x $cli ]] || { echo "installed CLI is not executable: $cli" >&2; exit 2; }
    cli=$(cd "$(dirname "$cli")" && pwd)/$(basename "$cli")
    ;;
  *)
    cli=$(command -v "$cli") || { echo "installed CLI is not executable: $1" >&2; exit 2; }
    ;;
esac

out=${2:-"$root/.scratch/smoke-release"}
fixtures="$root/fixtures"
[[ -d $fixtures/mantis-64/frames && -d $fixtures/mantis-512/frames ]] || { echo "missing smoke fixtures under: $fixtures" >&2; exit 2; }
[[ ! -e $out ]] || { echo "refusing to replace smoke output: $out" >&2; exit 2; }
python=$(command -v python3 || command -v python || true)
[[ -n $python ]] || { echo "python3 or python is required" >&2; exit 2; }

mkdir -p "$(dirname "$out")"
out=$(cd "$(dirname "$out")" && pwd)/$(basename "$out")
# Native Python and the stub CLI print paths that must open outside the MSYS root (`/tmp/...`).
if command -v cygpath >/dev/null 2>&1; then out=$(cygpath -m "$out"); cli=$(cygpath -m "$cli"); fi
mkdir -p "$out/input/mantis-64" "$out/input/mantis-512" "$out/logs" "$out/env/data" "$out/env/config" "$out/env/cache"
trap 'status=$?; printf "%s\\n" "$status" > "$out/exit-status"; exit "$status"' EXIT
uname -s > "$out/source-os"
cp "$fixtures/mantis-64/frames/"*.tif "$out/input/mantis-64/"
cp "$fixtures/mantis-512/frames/"*.tif "$out/input/mantis-512/"

run() {
  local label=$1
  shift
  printf '+ '
  printf '%q ' "$@"
  printf '\n'
  printf '+ ' >> "$out/commands.log"
  printf '%q ' "$@" >> "$out/commands.log"
  printf '\n' >> "$out/commands.log"
  if env FOCUSFORGE_DATA_DIR="$out/env/data" FOCUSFORGE_CONFIG_DIR="$out/env/config" XDG_DATA_HOME="$out/env/cache" XDG_CONFIG_HOME="$out/env/config" "$@" >"$out/logs/$label.stdout" 2>"$out/logs/$label.stderr"; then
    printf '0\n' > "$out/logs/$label.exit"
  else
    local status=$?
    printf '%s\n' "$status" > "$out/logs/$label.exit"
    return "$status"
  fi
}

if command -v sha256sum >/dev/null; then sha256sum "$cli" > "$out/installed-cli.sha256"; else shasum -a 256 "$cli" > "$out/installed-cli.sha256"; fi
run version "$cli" --version
mkdir -p "$out/projects"
run dry-run "$cli" stack "$out/input" --algorithm laplacian --cpu --no-align --dry-run --json --write-groups "$out/groups.toml" -o "$out/projects"
"$python" - "$out/logs/dry-run.stdout" "$out/groups.toml" <<'PY'
import json, pathlib, sys
report = json.load(open(sys.argv[1]))
groups = report.get("groups")
assert isinstance(groups, list) and len(groups) == 2
outputs = [item.get("would_write") for item in groups]
assert all(isinstance(item, list) and len(item) == 1 for item in outputs)
assert len({item[0] for item in outputs}) == 2
assert pathlib.Path(sys.argv[2]).is_file()
PY

run stack "$cli" stack "$out/input" --algorithm laplacian --cpu --no-align --json --groups "$out/groups.toml" --status "$out/status.ndjson" -o "$out/projects"
run review "$cli" review "$out/projects" --json
"$python" - "$out/projects/manifest.json" "$out/status.ndjson" "$out/logs/review.stdout" <<'PY'
import json, pathlib, sys
manifest = json.load(open(sys.argv[1]))["groups"]
status = [json.loads(line) for line in open(sys.argv[2]) if line.strip()]
review = json.load(open(sys.argv[3]))["results"]
assert len(manifest) == len(status) == len(review) == 2
expected = {item["identity"] for item in manifest}
assert len(expected) == 2
for records in (manifest, status, review):
    by_identity = {item["identity"]: item for item in records}
    assert set(by_identity) == expected and len(by_identity) == 2
    assert {item["outcome"] for item in by_identity.values()} == {"completed"}
for item in manifest:
    assert len(item["outputs"]) == 1 and pathlib.Path(item["outputs"][0]).is_file()
PY

project=$(find "$out/projects" -maxdepth 1 -name '*.ffp' -type f -print -quit)
[[ -n $project ]] || { echo "no project produced" >&2; exit 1; }
run export "$cli" export "$project" -o "$out/export.tif"
[[ -s $out/export.tif ]] || { echo "missing TIFF export" >&2; exit 1; }
run export-info "$cli" info --json "$out/export.tif"
"$python" - "$out/logs/stack.stdout" "$project" "$out/logs/export-info.stdout" <<'PY'
import json, pathlib, sys
stack = json.load(open(sys.argv[1]))
project = pathlib.Path(sys.argv[2]).resolve()
groups = stack.get("groups")
assert isinstance(groups, list)
matches = [group for group in groups if isinstance(group.get("outputs"), list) and any(pathlib.Path(output).resolve() == project for output in group["outputs"])]
assert len(matches) == 1
width, height = matches[0].get("output_width"), matches[0].get("output_height")
assert type(width) is int and type(height) is int and width > 0 and height > 0
report = json.load(open(sys.argv[3]))
files = report.get("files")
assert report.get("frame_count") == 1 and report.get("unreadable_count") == 0
assert isinstance(files, list) and len(files) == 1
assert (files[0].get("width"), files[0].get("height")) == (width, height)
PY

mkdir -p "$out/mixed/good" "$out/mixed/bad"
cp "$fixtures/mantis-64/frames/"*.tif "$out/mixed/good/"
printf 'corrupt TIFF' > "$out/mixed/bad/broken.tif"
mkdir -p "$out/mixed-projects"
if run mixed "$cli" stack "$out/mixed" --algorithm laplacian --cpu --no-align --json --status "$out/mixed-status.ndjson" -o "$out/mixed-projects"; then
  echo "mixed corrupt run unexpectedly succeeded" >&2
  exit 1
fi
run mixed-review "$cli" review "$out/mixed-projects" --json
"$python" - "$out/mixed-projects/manifest.json" "$out/mixed-status.ndjson" "$out/logs/mixed-review.stdout" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1]))["groups"]
status = [json.loads(line) for line in open(sys.argv[2]) if line.strip()]
review = json.load(open(sys.argv[3]))["results"]
assert len(manifest) == len(status) == len(review) == 2
expected = {item["identity"] for item in manifest}
expected_outcomes = {item["identity"]: item["outcome"] for item in manifest}
assert len(expected) == 2
assert set(expected_outcomes.values()) == {"completed", "failed"}
for records in (manifest, status, review):
    by_identity = {item["identity"]: item for item in records}
    assert set(by_identity) == expected and len(by_identity) == 2
    assert {identity: item["outcome"] for identity, item in by_identity.items()} == expected_outcomes
    failed = [item for item in by_identity.values() if item["outcome"] == "failed"]
    assert len(failed) == 1 and isinstance(failed[0].get("error"), str) and failed[0]["error"]
PY

echo "release smoke passed: $out"
