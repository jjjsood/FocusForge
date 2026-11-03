#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/push-package-repo.sh REMOTE BRANCH FILES_DIR MESSAGE
#
# Copies every file below FILES_DIR into BRANCH of the git repository REMOTE
# (the AUR package) at the same relative path, commits and
# pushes. Files of the repository that FILES_DIR does not name stay as they
# are. An unchanged result pushes nothing, so re-running a release is safe. An
# empty repository gets BRANCH as its first commit -- the first push to an AUR
# package name creates the package.
#
# Credentials come from the environment, never from arguments:
#   PACKAGE_REPO_SSH_KEY          private key for an ssh:// REMOTE
#   PACKAGE_REPO_SSH_FINGERPRINT  the host's published Ed25519 key fingerprint (SHA256:...);
#                                 the scanned host key must match it before anything connects
#   PACKAGE_REPO_TOKEN            token for an https:// REMOTE, sent as an HTTP header
# A local path or file:// REMOTE needs neither (the selftest uses one).
set -euo pipefail

[ "$#" -eq 4 ] || { echo "usage: $0 REMOTE BRANCH FILES_DIR MESSAGE" >&2; exit 2; }
remote=$1 branch=$2 files=$3 message=$4
[ -d "$files" ] || { echo "missing $files" >&2; exit 2; }
files=$(cd "$files" && pwd)
[ -n "$(find "$files" -type f -print -quit)" ] || { echo "$files holds no files" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case "$remote" in
  ssh://*)
    [ -n "${PACKAGE_REPO_SSH_KEY:-}" ] || { echo "PACKAGE_REPO_SSH_KEY is not set" >&2; exit 1; }
    [ -n "${PACKAGE_REPO_SSH_FINGERPRINT:-}" ] || { echo "PACKAGE_REPO_SSH_FINGERPRINT is not set" >&2; exit 1; }
    host=${remote#ssh://}; host=${host#*@}; host=${host%%/*}; host=${host%%:*}
    ssh-keyscan -t ed25519 "$host" 2>/dev/null > "$work/known_hosts"
    scanned=$(ssh-keygen -lf "$work/known_hosts" | awk '{print $2}')
    [ "$scanned" = "$PACKAGE_REPO_SSH_FINGERPRINT" ] || { echo "host key of $host is '$scanned', expected $PACKAGE_REPO_SSH_FINGERPRINT" >&2; exit 1; }
    (umask 077 && printf '%s\n' "$PACKAGE_REPO_SSH_KEY" > "$work/key")
    export GIT_SSH_COMMAND="ssh -i '$work/key' -o IdentitiesOnly=yes -o UserKnownHostsFile='$work/known_hosts' -o StrictHostKeyChecking=yes"
    ;;
  https://*)
    [ -n "${PACKAGE_REPO_TOKEN:-}" ] || { echo "PACKAGE_REPO_TOKEN is not set" >&2; exit 1; }
    auth=$(printf 'x-access-token:%s' "$PACKAGE_REPO_TOKEN" | base64 | tr -d '\n')
    # Through the environment, so the header never shows up in a process listing.
    export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader GIT_CONFIG_VALUE_0="Authorization: Basic $auth"
    ;;
esac

git clone --quiet "$remote" "$work/repo"
cd "$work/repo"
if git rev-parse --verify --quiet "refs/remotes/origin/$branch" >/dev/null; then
  git checkout --quiet -B "$branch" "origin/$branch"
elif [ -z "$(git for-each-ref refs/remotes)" ]; then
  git symbolic-ref HEAD "refs/heads/$branch"
else
  echo "$remote has commits but no branch $branch" >&2
  exit 1
fi

(cd "$files" && find . -type f -print0) | while IFS= read -r -d '' path; do
  mkdir -p "$(dirname "$path")"
  cp "$files/$path" "$path"
  git add -- "$path"
done
if git rev-parse --verify --quiet HEAD >/dev/null && git diff --cached --quiet; then
  echo "OK    $remote $branch already holds these files; nothing pushed"
  exit 0
fi
git -c user.name="github-actions[bot]" -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit --quiet -m "$message"
git push --quiet origin "HEAD:refs/heads/$branch"
echo "OK    pushed $(git rev-parse --short HEAD) to $remote $branch: $message"
