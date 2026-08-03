#!/usr/bin/env bash
# Pull upstream changes into this Linux-only fork.
#
# This repository deleted roughly a million lines of macOS, iOS and web code,
# so `git rebase upstream/main` is no longer mechanical: every upstream commit
# touching a deleted path produces a delete/modify conflict. Instead of
# replaying history, this script copies the paths this fork actually builds
# from a chosen upstream revision:
#
#   cmux-tui/                the Rust workspace
#   ghostty                  the submodule pointer
#   LICENSE                  upstream licence text, shipped in every package
#   THIRD_PARTY_LICENSES.md  upstream notices, shipped in every package
#
# Nothing under packaging/, docs/ or .github/ is touched.
#
# Usage:
#   packaging/linux/sync-upstream.sh [revision]     # default: upstream/main

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

REV="${1:-upstream/main}"
SYNC_PATHS=(cmux-tui ghostty LICENSE THIRD_PARTY_LICENSES.md)

cd "$REPO_ROOT"

git remote get-url upstream >/dev/null 2>&1 \
  || die "no 'upstream' remote. Add it: git remote add upstream https://github.com/manaflow-ai/cmux.git"

if [ -n "$(git status --porcelain)" ]; then
  die "working tree is dirty; commit or stash first"
fi

log "fetching upstream"
git fetch upstream --tags

git rev-parse --verify "$REV^{commit}" >/dev/null 2>&1 || die "unknown revision: $REV"
target="$(git rev-parse --short "$REV")"
log "syncing ${SYNC_PATHS[*]} from $REV ($target)"

before="$(git rev-parse HEAD)"
git checkout "$REV" -- "${SYNC_PATHS[@]}"

if git diff --cached --quiet; then
  log "already up to date with $REV"
  exit 0
fi

git diff --cached --stat | tail -20

cat <<EOF

Staged the upstream state of: ${SYNC_PATHS[*]}
Previous HEAD: $(git rev-parse --short "$before")
Upstream rev:  $target

Next:
  packaging/linux/build-all.sh          # rebuild and re-verify before committing
  git commit -m "sync cmux-tui from upstream $target"

If the ghostty submodule pointer moved, refresh the checkout:
  git submodule update --init --filter=blob:none ghostty
EOF
