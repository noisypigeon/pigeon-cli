#!/usr/bin/env bash
# Cut a release: bump Cargo.toml's patch version, cut CHANGELOG.md's
# [Unreleased] section, publish to crates.io, then commit+tag+push --
# publish runs before the git push so a failed publish never leaves main
# with a version crates.io doesn't have. See docs/adr/0117.
#
# Invoked as a single-line passthrough from `mise run release -- "$@"`
# rather than parsing "$@" inline in .mise.toml -- mise appends extra CLI
# args as literal text at the end of the rendered run command rather than
# populating a real "$@" throughout a multi-line task script, so argument
# parsing has to live in an actual script, not inline TOML.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: release.sh [--dry-run]

  --dry-run   Still bumps Cargo.toml/CHANGELOG.md on disk and runs
              `cargo publish --dry-run`, but skips the real publish and
              the git commit/tag/push. Discard the on-disk bump afterward
              with: git checkout -- Cargo.toml Cargo.lock CHANGELOG.md
EOF
}

dry_run=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 1 ;;
  esac
done

release_date="$(date -u +%Y-%m-%d)"
new_version_line="$(.github/scripts/cut-release.sh --date "$release_date" | tail -1)"
new_version="${new_version_line#NEW_VERSION=}"

if [ "$dry_run" -eq 1 ]; then
  cargo publish --dry-run --allow-dirty
  echo "Dry run complete for v${new_version}. Cargo.toml/Cargo.lock/CHANGELOG.md" >&2
  echo "were modified on disk but not committed. Run:" >&2
  echo "  git checkout -- Cargo.toml Cargo.lock CHANGELOG.md" >&2
  echo "to discard." >&2
  echo "NEW_VERSION=${new_version}"
  exit 0
fi

cargo publish --allow-dirty

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(release): v${new_version}"
git tag "v${new_version}"
git push origin HEAD:main
git push origin "v${new_version}"

echo "NEW_VERSION=${new_version}"
