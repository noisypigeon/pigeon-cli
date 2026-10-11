#!/usr/bin/env bash
# Bump Cargo.toml's patch version and cut CHANGELOG.md's [Unreleased]
# section into a dated release section. See docs/adr/0117.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: cut-release.sh --date YYYY-MM-DD

  --date   Release date to stamp the new CHANGELOG.md section with, e.g.
           2026-10-10. Pass `$(date -u +%Y-%m-%d)` from the caller -- this
           script never reads the clock itself.

Always prints "NEW_VERSION=<version>" as the final line of stdout.
EOF
}

release_date=""
while [ $# -gt 0 ]; do
  case "$1" in
    --date) release_date="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 1 ;;
  esac
done

if [ -z "$release_date" ]; then
  echo "--date is required." >&2
  usage >&2
  exit 1
fi

if ! [[ "$release_date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
  echo "--date must be YYYY-MM-DD, got: $release_date" >&2
  exit 1
fi

current_version="$(awk -F'"' '
  /^\[package\]/ { in_pkg=1; next }
  /^\[/          { in_pkg=0 }
  in_pkg && /^version[[:space:]]*=/ { print $2; exit }
' Cargo.toml)"

if [ -z "$current_version" ]; then
  echo "Could not find [package] version in Cargo.toml." >&2
  exit 1
fi

if ! [[ "$current_version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  echo "Cargo.toml version '$current_version' is not plain x.y.z; refusing to guess a patch bump." >&2
  exit 1
fi

major="${BASH_REMATCH[1]}"
minor="${BASH_REMATCH[2]}"
patch="${BASH_REMATCH[3]}"
new_version="${major}.${minor}.$((patch + 1))"

# Refuse to cut a release with nothing in it -- every merged PR appends a
# bullet before merge per ADR-0029 step 4, so this should only ever fire if
# main was pushed to directly, bypassing that process.
if ! awk '
  /^## \[Unreleased\]$/ { found=1; next }
  found && /^## \[/ { exit }
  found && /^- / { seen=1 }
  END { exit !seen }
' CHANGELOG.md; then
  echo "CHANGELOG.md [Unreleased] has no bullets; nothing to release." >&2
  exit 1
fi

awk -v newver="$new_version" '
  /^\[package\]/ { print; in_pkg=1; next }
  /^\[/          { in_pkg=0 }
  in_pkg && /^version[[:space:]]*=/ && !done {
    print "version = \"" newver "\""
    done=1
    next
  }
  { print }
' Cargo.toml > Cargo.toml.new
mv Cargo.toml.new Cargo.toml

awk -v newver="$new_version" -v date="$release_date" '
  /^## \[Unreleased\]$/ && !done {
    print
    print ""
    print "## [" newver "] - " date
    done=1
    next
  }
  { print }
' CHANGELOG.md > CHANGELOG.md.new
mv CHANGELOG.md.new CHANGELOG.md

# Resync Cargo.lock's own pigeon-cli entry to the bumped version. Cheap --
# target/ is already warm from `mise run ci` moments earlier in this job.
cargo check --quiet

echo "NEW_VERSION=${new_version}"
