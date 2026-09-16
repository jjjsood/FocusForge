#!/usr/bin/env bash
# Usage: tools/with-lock.sh <build|gpu> -- <command> [args...]
#
# Command-line front end for tools/lib/lock.sh -- see there for what each
# lock actually guards and why. Use this to wrap any cargo invocation, any
# direct `target/{debug,release}/focusforge-cli` run, or any GPU-touching
# command you are about to type by hand, so it serializes against other
# builds and GPU runs on this machine.
#
# Exit code: the wrapped command's own, or 2 on a usage error.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE/.."

usage() {
  echo "usage: tools/with-lock.sh <build|gpu> -- <command> [args...]" >&2
  exit 2
}

[ "$#" -ge 3 ] || usage
case "$1" in
  build|gpu) ;;
  *) usage ;;
esac
[ "$2" = "--" ] || usage

# shellcheck source=lib/lock.sh
. "$HERE/lib/lock.sh"
with_lock "$@"
exit $?
