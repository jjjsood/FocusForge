#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
product="$work/product with spaces"
mkdir -p "$product/tools"
cp "$root/tools/smoke-release.sh" "$product/tools/"
helper="$product/tools/smoke-release.sh"

expect_fail() {
  local label=$1
  shift
  if "$@" >"$work/$label.stdout" 2>"$work/$label.stderr"; then
    echo "expected failure: $label" >&2
    return 1
  fi
}

make_fixtures() {
  local fixtures=$1
  mkdir -p "$fixtures/mantis-64/frames" "$fixtures/mantis-512/frames"
  printf '64' > "$fixtures/mantis-64/frames/00.tif"
  printf '512' > "$fixtures/mantis-512/frames/00.tif"
}

make_cli() {
  local cli=$1
  cat > "$cli" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
mode=${MOCK_SMOKE_MODE:-valid}
case $1 in
  --version) echo 'focusforge-cli 0.1.0' ;;
  stack)
    shift
    input=$1
    shift
    output= groups= status= dry=0
    while (($#)); do
      case $1 in
        -o) output=$2; shift 2 ;;
        --write-groups) groups=$2; shift 2 ;;
        --status) status=$2; shift 2 ;;
        --dry-run) dry=1; shift ;;
        *) shift ;;
      esac
    done
    [[ -d $output ]] || exit 70
    if [[ -n $groups ]]; then
      printf '[[group]]\nname = "mantis-64"\nframes = ["mantis-64/frames/00.tif"]\n\n[[group]]\nname = "mantis-512"\nframes = ["mantis-512/frames/00.tif"]\n' > "$groups"
    fi
    if ((dry)); then
      if [[ $mode == malformed ]]; then printf '{'; elif [[ $mode == dry-wrong-count ]]; then printf '{"groups":[{"would_write":["%s/mantis-64.ffp"]}]}\n' "$output"; else printf '{"groups":[{"would_write":["%s/mantis-64.ffp"]},{"would_write":["%s/mantis-512.ffp"]}]}\n' "$output" "$output"; fi
      exit 0
    fi
    if [[ $input == */mixed ]]; then
      : > "$output/mantis-64.ffp"
      if [[ $mode == dropped-failed ]]; then
        printf '{"groups":[{"identity":"mantis-64","outcome":"completed","outputs":["%s/mantis-64.ffp"],"error":null}]}' "$output" > "$output/manifest.json"
        printf '%s\n' '{"identity":"mantis-64","outcome":"completed","error":null}' > "$status"
      elif [[ $mode == swapped-outcomes ]]; then
        printf '{"groups":[{"identity":"mantis-64","outcome":"completed","outputs":["%s/mantis-64.ffp"],"error":null},{"identity":"broken","outcome":"failed","outputs":[],"error":"invalid TIFF"}]}' "$output" > "$output/manifest.json"
        printf '%s\n' '{"identity":"mantis-64","outcome":"failed","error":"invalid TIFF"}' '{"identity":"broken","outcome":"completed","error":null}' > "$status"
      elif [[ $mode == cancelled-outcome ]]; then
        printf '{"groups":[{"identity":"mantis-64","outcome":"cancelled","outputs":["%s/mantis-64.ffp"],"error":null},{"identity":"broken","outcome":"failed","outputs":[],"error":"invalid TIFF"}]}' "$output" > "$output/manifest.json"
        printf '%s\n' '{"identity":"mantis-64","outcome":"cancelled","error":null}' '{"identity":"broken","outcome":"failed","error":"invalid TIFF"}' > "$status"
      else
        printf '{"groups":[{"identity":"mantis-64","outcome":"completed","outputs":["%s/mantis-64.ffp"],"error":null},{"identity":"broken","outcome":"failed","outputs":[],"error":"invalid TIFF"}]}' "$output" > "$output/manifest.json"
        printf '%s\n' '{"identity":"mantis-64","outcome":"completed","error":null}' '{"identity":"broken","outcome":"failed","error":"invalid TIFF"}' > "$status"
      fi
      exit 1
    fi
    : > "$output/mantis-64.ffp"
    [[ $mode == missing-project ]] || : > "$output/mantis-512.ffp"
    identity2=mantis-512
    [[ $mode == duplicate-identity ]] && identity2=mantis-64
    printf '{"groups":[{"identity":"mantis-64","outcome":"completed","outputs":["%s/mantis-64.ffp"],"error":null},{"identity":"%s","outcome":"completed","outputs":["%s/mantis-512.ffp"],"error":null}]}' "$output" "$identity2" "$output" > "$output/manifest.json"
    status2=$identity2
    [[ $mode == mismatched-identity ]] && status2=unexpected
    printf '{"identity":"mantis-64","outcome":"completed","error":null}\n{"identity":"%s","outcome":"completed","error":null}\n' "$status2" > "$status"
    printf '{"groups":[{"outputs":["%s/mantis-64.ffp"],"output_width":60,"output_height":60},{"outputs":["%s/mantis-512.ffp"],"output_width":508,"output_height":508}]}\n' "$output" "$output"
    ;;
  export)
    target=${@: -1}
    if [[ $mode != missing-export-tiff ]]; then
      printf TIFF > "$target"
      case $2 in *mantis-512.ffp) printf 508 > "$target.dim" ;; *) printf 60 > "$target.dim" ;; esac
    fi
    ;;
  info)
    dimension=$(cat "${@: -1}.dim")
    if [[ $mode == mismatched-export-extent ]]; then printf '{"frame_count":1,"unreadable_count":0,"files":[{"width":61,"height":%s}]}\n' "$dimension"; elif [[ $mode == unreadable-info ]]; then printf '{"frame_count":1,"unreadable_count":1,"files":[{"width":%s,"height":%s}]}\n' "$dimension" "$dimension"; else printf '{"frame_count":1,"unreadable_count":0,"files":[{"width":%s,"height":%s}]}\n' "$dimension" "$dimension"; fi
    ;;
  review)
    if [[ $2 == */mixed-projects ]]; then
      if [[ $mode == dropped-failed ]]; then printf '{"results":[{"identity":"mantis-64","outcome":"completed"}]}\n'; elif [[ $mode == swapped-outcomes ]]; then printf '{"results":[{"identity":"mantis-64","outcome":"failed","error":"invalid TIFF"},{"identity":"broken","outcome":"completed"}]}\n'; elif [[ $mode == cancelled-outcome ]]; then printf '{"results":[{"identity":"mantis-64","outcome":"cancelled"},{"identity":"broken","outcome":"failed","error":"invalid TIFF"}]}\n'; else printf '{"results":[{"identity":"mantis-64","outcome":"completed"},{"identity":"broken","outcome":"failed","error":"invalid TIFF"}]}\n'; fi
    elif [[ $mode == wrong-count ]]; then printf '{"results":[{}]}\n'; elif [[ $mode == duplicate-identity ]]; then printf '{"results":[{"identity":"mantis-64","outcome":"completed"},{"identity":"mantis-64","outcome":"completed"}]}\n'; else printf '{"results":[{"identity":"mantis-64","outcome":"completed"},{"identity":"mantis-512","outcome":"completed"}]}\n'; fi
    ;;
  *) exit 64 ;;
esac
STUB
  chmod +x "$cli"
}

fixtures="$product/fixtures"
cli="$work/focusforge-cli.exe"
make_fixtures "$fixtures"
make_cli "$cli"
"$helper" "$cli" "$work/valid output"
expect_fail missing-cli "$helper" "$work/missing" "$work/out"
missing_product="$work/missing product"
mkdir -p "$missing_product/tools"
cp "$root/tools/smoke-release.sh" "$missing_product/tools/"
expect_fail missing-fixtures "$missing_product/tools/smoke-release.sh" "$cli" "$work/out"
MOCK_SMOKE_MODE=malformed expect_fail malformed "$helper" "$cli" "$work/malformed"
MOCK_SMOKE_MODE=dry-wrong-count expect_fail dry-wrong-count "$helper" "$cli" "$work/dry-wrong-count"
MOCK_SMOKE_MODE=missing-project expect_fail missing-project "$helper" "$cli" "$work/missing-project"
MOCK_SMOKE_MODE=wrong-count expect_fail wrong-count "$helper" "$cli" "$work/wrong-count"
MOCK_SMOKE_MODE=duplicate-identity expect_fail duplicate-identity "$helper" "$cli" "$work/duplicate-identity"
MOCK_SMOKE_MODE=mismatched-identity expect_fail mismatched-identity "$helper" "$cli" "$work/mismatched-identity"
MOCK_SMOKE_MODE=dropped-failed expect_fail dropped-failed "$helper" "$cli" "$work/dropped-failed"
MOCK_SMOKE_MODE=swapped-outcomes expect_fail swapped-outcomes "$helper" "$cli" "$work/swapped-outcomes"
MOCK_SMOKE_MODE=cancelled-outcome expect_fail cancelled-outcome "$helper" "$cli" "$work/cancelled-outcome"
MOCK_SMOKE_MODE=mismatched-export-extent expect_fail mismatched-export-extent "$helper" "$cli" "$work/mismatched-export-extent"
MOCK_SMOKE_MODE=unreadable-info expect_fail unreadable-info "$helper" "$cli" "$work/unreadable-info"
MOCK_SMOKE_MODE=missing-export-tiff expect_fail missing-export-tiff "$helper" "$cli" "$work/missing-export-tiff"
echo "smoke release selftest: PASS"
