#!/usr/bin/env bash
# Cut a release: bump Cargo.toml's patch version, cut CHANGELOG.md's
# [Unreleased] section, commit+tag+push, then publish to crates.io --
# push runs BEFORE publish so a merge that lands mid-run defers this
# release instead of burning a crates.io version. See adr/CLI-0150,
# which reverses adr/CLI-0117's original publish-then-push ordering.
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

  --dry-run   Still bumps Cargo.toml/CHANGELOG.md on disk, reports the
              crates.io pre-check's verdict, and runs `cargo publish
              --dry-run`, but skips the real publish and the git
              commit/tag/push. Discard the on-disk bump afterward with:
              git checkout -- Cargo.toml Cargo.lock CHANGELOG.md
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

# CLI-0150 §3: is this version already on crates.io? Answers via the sparse
# index -- one newline-delimited JSON record per published version, no auth,
# no tooling beyond curl, and no registry mutation.
#
# An unreachable index returns "unknown", NOT "absent": a network blip must
# never silently suppress a real release, so the caller attempts the publish
# and lets `cargo publish`'s own error be the authority.
crate_version_published() {
  local version="$1"
  local index_url="https://index.crates.io/pi/ge/pigeon-cli"
  local body
  if ! body="$(curl -fsSL --max-time 30 "$index_url" 2>/dev/null)"; then
    echo "unknown"
    return
  fi
  if printf '%s\n' "$body" | grep -q "\"vers\":\"${version}\""; then
    echo "present"
  else
    echo "absent"
  fi
}

release_date="$(date -u +%Y-%m-%d)"
new_version_line="$(.github/scripts/cut-release.sh --date "$release_date" | tail -1)"
new_version="${new_version_line#NEW_VERSION=}"

published="$(crate_version_published "$new_version")"

if [ "$dry_run" -eq 1 ]; then
  echo "crates.io pre-check for v${new_version}: ${published}" >&2
  cargo publish --dry-run --allow-dirty
  echo "Dry run complete for v${new_version}. Cargo.toml/Cargo.lock/CHANGELOG.md" >&2
  echo "were modified on disk but not committed. Run:" >&2
  echo "  git checkout -- Cargo.toml Cargo.lock CHANGELOG.md" >&2
  echo "to discard." >&2
  echo "NEW_VERSION=${new_version}"
  exit 0
fi

# CLI-0150 §1: commit, tag, and push BEFORE publishing. A rejected push
# costs nothing here -- nothing has been published, main is untouched, the
# on-disk bump dies with the runner, and the accumulated [Unreleased]
# bullets roll into the next merge's release.
#
# Deliberately not --force-with-lease: a rejection is exactly the signal we
# want, and forcing would clobber the concurrent merge that caused it.
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(release): v${new_version}"
git tag "v${new_version}"

if ! git push origin HEAD:main; then
  echo "" >&2
  echo "Push to main was rejected -- a merge landed while this release was" >&2
  echo "building, so this release is deferred (CLI-0150 §1). Nothing was" >&2
  echo "published and main is unchanged; the next merge will cut it." >&2
  exit 1
fi
git push origin "v${new_version}"

# CLI-0150 §3: skip rather than die if this version is somehow already
# published -- that is what wedged the pipeline for five merges before this
# ADR. The tree is clean by now (the bump is committed above), so no
# --allow-dirty: `cargo publish`'s uncommitted-changes check is live again
# for the first time since CLI-0117.
if [ "$published" = "present" ]; then
  echo "" >&2
  echo "WARNING: pigeon-cli@${new_version} is already on crates.io; skipping" >&2
  echo "publish (CLI-0150 §3). The release commit and tag were still pushed." >&2
else
  cargo publish
fi

echo "NEW_VERSION=${new_version}"
