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

# Re-apply the fork's patches against upstream files. Checking out upstream
# paths discards them, so a sync that skipped this would silently revert fixes
# the GUI depends on.
shopt -s nullglob
patches=("$REPO_ROOT"/patches/*.patch)
shopt -u nullglob
for patch in "${patches[@]}"; do
  name="$(basename "$patch")"
  if git apply --check "$patch" 2>/dev/null; then
    git apply "$patch"
    git add -u
    log "applied $name"
  elif git apply --reverse --check "$patch" 2>/dev/null; then
    # Already present in the synced tree: upstream took the fix, so the patch
    # can be deleted rather than carried.
    log "$name is already upstream — delete it from patches/"
  else
    die "$name no longer applies to $REV; rebase or drop it before continuing"
  fi
done

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
