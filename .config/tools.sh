#!/usr/bin/env bash
# Clones noisypigeon/runway, whose tools/ is the library (LIB-0047, LIB-0050),
# into .config/src/ at the tag .config/version names. Set PIGEON_SRC to use
# your own checkout instead.
set -euo pipefail

PIGEON_REPO_URL="${PIGEON_REPO_URL:-https://github.com/noisypigeon/runway.git}"
REPO_ROOT="$(git rev-parse --show-toplevel)"

if [ -n "${PIGEON_SRC:-}" ]; then
  if [ ! -d "$PIGEON_SRC/tools/github/scripts" ]; then
    echo "::error::PIGEON_SRC=$PIGEON_SRC has no tools/github/scripts/ directory" >&2
    exit 1
  fi
  echo "Library resolved -> $PIGEON_SRC (caller-supplied, left as-is)"
  exit 0
fi

REF="$(tr -d '[:space:]' < "$REPO_ROOT/.config/version")"
if [ -z "$REF" ]; then
  echo "::error::$REPO_ROOT/.config/version is empty; it must name a library tag" >&2
  exit 1
fi

SRC="$REPO_ROOT/.config/src"

if [ -d "$SRC/.git" ]; then
  git -C "$SRC" fetch --quiet --tags --prune --force origin
else
  rm -rf "$SRC"
  git clone --quiet --filter=blob:none "$PIGEON_REPO_URL" "$SRC"
fi

if ! git -C "$SRC" rev-parse --verify --quiet "$REF" >/dev/null; then
  echo "::error::library has no ref $REF (from .config/version)" >&2
  exit 1
fi
git -C "$SRC" checkout --quiet --detach "$REF"

echo "Library resolved -> ${SRC#"$REPO_ROOT"/} @ ${REF} ($(git -C "$SRC" rev-parse --short HEAD), $(git -C "$SRC" tag | wc -l | tr -d ' ') tags)"
