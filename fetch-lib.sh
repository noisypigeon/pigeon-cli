#!/usr/bin/env bash
# Materializes the library repo -- noisypigeon/library, the hub of LIB-0001's
# hub-and-spoke model -- into a gitignored /.lib-src/ at this repo's root,
# checked out at the version /.lib-version pins. Copied from noisypigeon/blog's
# by CLI-0161, changing only this comment and the LIB_SRC sanity check.
#
# This repo reads three things out of it: .github/scripts/crate-*.sh and
# adr-issue.sh (the release pipeline and `mise run adr-issue`), the
# observability/pigeon-cli stack at the tag .mise.toml pins, and nothing else --
# the reusable release workflow is pinned by its own `uses:` ref, and the
# Claude Code plugin by .claude/settings.json.
#
# Pinned, not branch-tracking: LIB-0001 invariant 2 says a spoke consumes
# versions, never branches.
#
# Tags are always fetched regardless of the pinned ref, because the
# observability tasks extract a stack version by tag out of this clone.
#
# Blobless (--filter=blob:none) because the common case only needs the tag graph
# and a handful of files; blobs are fetched on demand.
#
# This script is the one thing a spoke cannot fetch -- it is the fetcher -- so it
# is the deliberate exception to "a spoke contains no scripts" (LIB-0002). Set
# LIB_SRC to point at your own checkout and it leaves that alone entirely.
set -euo pipefail

LIB_REPO_URL="${LIB_REPO_URL:-https://github.com/noisypigeon/library.git}"
REPO_ROOT="$(git rev-parse --show-toplevel)"

if [ -n "${LIB_SRC:-}" ]; then
  if [ ! -d "$LIB_SRC/.github/scripts" ]; then
    echo "::error::LIB_SRC=$LIB_SRC has no .github/scripts/ directory" >&2
    exit 1
  fi
  echo "Library resolved -> $LIB_SRC (caller-supplied, left as-is)"
  exit 0
fi

LIB_REF="$(tr -d '[:space:]' < "$REPO_ROOT/.lib-version")"
if [ -z "$LIB_REF" ]; then
  echo "::error::$REPO_ROOT/.lib-version is empty; it must name a library ref" >&2
  exit 1
fi

LIB_SRC="$REPO_ROOT/.lib-src"

if [ -d "$LIB_SRC/.git" ]; then
  git -C "$LIB_SRC" fetch --quiet --tags --prune --force origin
else
  rm -rf "$LIB_SRC"
  git clone --quiet --filter=blob:none "$LIB_REPO_URL" "$LIB_SRC"
fi

if ! git -C "$LIB_SRC" rev-parse --verify --quiet "$LIB_REF" >/dev/null; then
  echo "::error::library has no ref $LIB_REF (from .lib-version)" >&2
  exit 1
fi
git -C "$LIB_SRC" checkout --quiet --detach "$LIB_REF"

echo "Library resolved -> ${LIB_SRC#"$REPO_ROOT"/} @ ${LIB_REF} ($(git -C "$LIB_SRC" rev-parse --short HEAD), $(git -C "$LIB_SRC" tag | wc -l | tr -d ' ') tags)"
