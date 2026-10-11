# ADR-0117: automate crates.io release and binary publishing on every merge to `main`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

Publishing `pigeon-cli` today is entirely manual, per ADR-0018: hand-bump
`Cargo.toml`'s `version`, hand-retitle `CHANGELOG.md`'s `[Unreleased]`
section into a dated version section, run `mise run publish` (`cargo
publish`) locally. ADR-0018 explicitly rejected both automated
version-bump tooling and any CI/CD automation for publishing as out of
scope, and ADR-0029 §6 explicitly says "releases stay manual." There is
also no GitHub Actions workflow of any kind in this repo yet — `.github/`
holds only `.github/scripts/adr-issue.sh`; `mise run ci` (fmt-check + lint
+ test) is a purely local gate the developer runs by hand before opening a
PR, with no server-side check at all.

This ADR automates that entire flow: **every PR merge to `main` produces
an automatic crates.io release**, with no manual version bump, changelog
cut, or `cargo publish` step. It's the first ADR to introduce GitHub
Actions to this repo, and reverses the relevant parts of both ADR-0018 and
ADR-0029 rather than silently diverging from them.

Confirmed repo state at the time of writing: `Cargo.toml` was at `0.4.0`;
`CHANGELOG.md`'s latest cut section was `[0.4.0] - 2026-10-04`, with
`[Unreleased]` holding roughly 17 bullets (ADR-0102 through ADR-0115)
accumulated since, each already shaped
`- ADR-XXXX: <desc> ([#N](url))` per ADR-0029 §4. No branch protection on
`main`. Squash-merge via `gh pr merge --squash --delete-branch` is the
only merge path, so every landed PR is exactly one commit on `main`.

The `#35` out-of-scope references in both ADR-0018 and ADR-0029 point at
`github.com/noisypigeon/pigeon/issues/35` — the pre-split repo's issue
tracker (per ADR-0084's repo split and ADR-0086's rename-back). That issue
number doesn't carry over; there is nothing live in the current
`noisypigeon/pigeon-cli` repo to close by landing this ADR.

**`0.4.1` was already published and tagged manually while implementing
this ADR**, exercising the exact `release.sh` mechanism this ADR
describes (bump → cut changelog → `cargo publish` → commit/tag/push) via
a local `mise run release` invocation during testing, rather than via the
GitHub Actions workflow — that workflow didn't exist on `main` yet at the
time. `CHANGELOG.md`'s `[Unreleased]` is therefore empty again as of this
writing, and `Cargo.toml` is at `0.4.1`. Stated here plainly as part of
this ADR's own history rather than silently smoothed over. One real
consequence: `CARGO_REGISTRY_TOKEN` is confirmed to already exist as a
valid, working local `cargo login` credential (it was used for that real
publish) — but it is **not yet present as a GitHub Actions secret**, which
is still a precondition for this PR to merge (see below).

**This ADR's own merge performs the first *automated* (CI-triggered)
release** — the workflow file lands on `main` as part of this PR's
squash-merge commit, and that same push is what triggers it. That first
automated run coalesces whatever is in `[Unreleased]` at merge time
(starting from the `0.4.1` baseline above) into `0.4.1 → 0.4.2`. Stated
here explicitly so it isn't a surprise when it happens.

## Decision

### Every merge bumps patch, cuts the changelog, and publishes — in that order

`.github/workflows/release.yml` (new) triggers `on: push: branches:
[main]`. Its `release` job:

1. Runs `mise run ci` once more server-side (defense-in-depth; the local
   gate already ran pre-merge, but this is the first time anything in this
   repo runs CI on `main` itself).
2. Runs the new `mise run release` task, which:
   a. Reads whatever version is *currently* in `Cargo.toml` and bumps only
      its patch segment by 1 — always, unconditionally, regardless of how
      that version got there. A deliberate minor/major bump stays a manual
      `Cargo.toml` edit inside a PR; this rule never looks at history, so
      it simply operates off that new baseline on the next merge, no
      special-casing required.
   b. Cuts `CHANGELOG.md`'s `[Unreleased]` section into
      `## [<new-version>] - <date>`, leaving a fresh empty `[Unreleased]`
      above it.
   c. Runs `cargo publish --allow-dirty` against the bumped, uncommitted
      tree.
   d. Only after a successful publish: commits the three changed files
      (`Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`) as
      `chore(release): v<version>`, tags `v<version>`, and pushes both to
      `main`.
3. Creates a GitHub Release at that tag (`gh release create`).

**Publish runs before the commit/tag/push, not after.** If `cargo publish`
fails (network blip, registry outage, a transient verification failure),
`main` stays untouched and simply re-running the job is a safe, idempotent
retry. Pushing the bump to `main` first and having publish fail afterward
would leave `main` showing a version crates.io never received, and a naive
retry would double-bump since `Cargo.toml` on `main` would have already
moved.

Because the version bump happens on disk before publish and is only
committed after a successful publish, the working tree is guaranteed dirty
at publish time — `cargo publish` therefore runs with `--allow-dirty`. The
only uncommitted state at that point is exactly the three files the
release step just touched.

### Loop safety and concurrency

GitHub Actions' default `GITHUB_TOKEN` pushes don't retrigger `on: push`
workflows at all (built-in anti-recursion) — there is no actual
infinite-loop risk from this workflow pushing its own commit back to
`main`. An explicit job-level guard is still added as cheap
defense-in-depth and documentation, in case a PAT is ever substituted
later for some other reason:

```yaml
if: ${{ !startsWith(github.event.head_commit.message, 'chore(release): v') }}
```

A `concurrency: { group: release-main, cancel-in-progress: false }` block
at the workflow level prevents two near-simultaneous merges from racing
the release push. Because merges are squash-only and linear (confirmed via
`git log` and ADR-0029 §5), a superseded *queued* run is never lossy — the
run that does execute checks out a commit that transitively includes
everything the skipped run would have seen, and coalesces all accumulated
`[Unreleased]` bullets into one release. Fewer release commits than merges
during a burst is intentional, not a bug.

### `.github/scripts/cut-release.sh` (new)

A bash script matching `adr-issue.sh`'s existing conventions (`set -euo
pipefail`, a `usage()` function, long-flag parsing), taking `--date
YYYY-MM-DD` — the caller passes `$(date -u +%Y-%m-%d)`; the script never
reads the clock itself, consistent with this repo's explicit-input style.
It:

1. Parses `Cargo.toml`'s `[package]`-scoped `version = "x.y.z"` via `awk`
   (scoped to the `[package]` section specifically, not a bare `grep`, so
   a future `[workspace.package]` section can't collide). Rejects anything
   not plain `x.y.z` loudly rather than guessing.
2. Guards: if `CHANGELOG.md`'s `[Unreleased]` section has zero `- `
   bullets, exits with an error — defends against a direct, non-PR push to
   `main` bypassing ADR-0029 §4's per-PR bullet and triggering a
   content-free release.
3. Bumps the patch segment; rewrites `Cargo.toml`'s version line and
   inserts `## [<new-version>] - <date>` right after `## [Unreleased]`,
   leaving a fresh empty `[Unreleased]` above it — both via
   `awk ... > tmp && mv tmp original` rather than `sed -i` (which differs
   between BSD/macOS and GNU/Linux, and this script runs on both: locally
   via `mise run release -- --dry-run`, and on the Ubuntu Actions runner).
4. Runs `cargo check --quiet` to resync `Cargo.lock`'s own `pigeon-cli`
   entry (cheap — `target/` is already warm from `mise run ci` moments
   earlier in the same job).
5. Prints `NEW_VERSION=<version>` as the final stdout line for the caller
   to capture.

### `.github/scripts/release.sh` (new) and `.mise.toml` — new `release` task

Wraps `cut-release.sh` plus the publish/commit/tag/push sequence, with a
`--dry-run` flag: still bumps `Cargo.toml`/cuts `CHANGELOG.md` on disk and
runs `cargo publish --dry-run`, but skips the real publish and the git
commit/tag/push, leaving the bump on disk uncommitted with a printed `git
checkout -- Cargo.toml Cargo.lock CHANGELOG.md` discard hint — so the
whole flow is runnable and testable locally exactly like
`publish`/`publish-dry-run` are today. Commit message format:
`chore(release): v<version>` — deliberately namespaced away from
ADR-0029's `type(adr-XXXX): summary` convention, and matched by the
workflow's loop-guard condition above.

This logic lives in its own script (`.github/scripts/release.sh`), not
inlined in `.mise.toml`, for a concrete reason found while implementing
this ADR: mise's `run` tasks don't populate a real `"$@"` for extra CLI
args throughout a multi-line task script — it appends them as literal
text at the very end of the *rendered* command line, which only lands
correctly when the task body is a single line ending in `"$@"` (the
pattern `adr-issue` already uses). `.mise.toml`'s `release` task is
therefore a one-line passthrough, `.github/scripts/release.sh "$@"`,
matching that existing precedent; all the actual argument parsing happens
inside the script, where `"$@"` behaves normally.

### Secret: `CARGO_REGISTRY_TOKEN`

Must be added manually via the repo's GitHub Settings → Secrets *before*
this PR merges (or at least before its first run reaches the publish
step) — mirroring ADR-0018's manual `cargo login` precedent; this ADR's
automation cannot provision it itself. Must belong to the crates.io
account that already owns `pigeon-cli` from prior manual ADR-0018-era
publishes. If missing when the first run fires, the publish-before-push
ordering means it fails cleanly at the publish step with `main` untouched
— add the secret and re-run the failed job, no new commit needed.

### Also build and attach precompiled binaries to a GitHub Release

`cargo publish` only uploads source (per `Cargo.toml`'s `include` list) —
`cargo install pigeon-cli` always recompiles from source on the
installer's own machine. To move compilation to release time instead of
install/runtime, the same workflow's `build-binaries` job (`needs:
release`, so it's automatically skipped whenever `release` is) builds
native binaries on GitHub's **native arm64-hosted runners** — no
cross-compilation or Docker-image extraction needed:

```yaml
strategy:
  matrix:
    include:
      - runner: macos-14
        triple: aarch64-apple-darwin
      - runner: ubuntu-24.04-arm
        triple: aarch64-unknown-linux-gnu
runs-on: ${{ matrix.runner }}
```

- **macOS arm64** (`aarch64-apple-darwin`) matches the dev machine; the
  existing `mise run build-release` task already conditionally ad-hoc
  codesigns on Darwin.
- **Linux arm64** (`aarch64-unknown-linux-gnu`) matches the existing
  `docker-build`/`docker-run` mise tasks' `linux/arm64` target.

Each leg checks out the new tag, runs `mise run build-release` unchanged,
packages `target/release/pigeon` plus `LICENSE.md` into
`pigeon-v<version>-<triple>.tar.gz`, and uploads it via `gh release upload`
to the release the `release` job already created. `GITHUB_TOKEN`'s
`contents: write` permission covers release/asset creation, so no new
secret is needed for this half.

**Known limitation, stated rather than silently glossed over**: the macOS
binary only carries the existing ad-hoc codesign (`codesign -s -`, no
Developer ID or notarization). A binary downloaded via browser/curl picks
up Gatekeeper's quarantine attribute and will likely still show "cannot be
opened, unidentified developer" until the user runs `xattr -d
com.apple.quarantine` on it, or right-click-opens it once. Full
notarization requires a paid Apple Developer ID and is out of scope here.

## Consequences

- Every merged PR now results in a real, automatic crates.io release and a
  GitHub Release carrying two precompiled binaries — no more manual
  `mise run publish` step, ever, for an ordinary PR.
- This ADR's own merge performs the first *automated* release, bumping
  `0.4.1 → 0.4.2` (the `0.4.0 → 0.4.1` bump already happened manually
  during implementation — see Context).
- `main` gains occasional bot-authored `chore(release): vX.Y.Z` commits
  with no corresponding PR of their own — a narrow, explicitly-noted
  exception to ADR-0029's "every substantive change goes through a PR":
  this is release bookkeeping, not a substantive change, and there's no
  way to open a PR for the commit that releases the thing a PR would
  contain.
- `.github/workflows/` exists in this repo for the first time.
- A burst of merges landing close together produces fewer releases than
  merges (each coalescing whatever accumulated in `[Unreleased]`) rather
  than one release per merge — an accepted consequence of the
  `cancel-in-progress: false` concurrency group, not a bug.

## Out of scope

- A PR-triggered lint/test CI workflow — server-side enforcement of `mise
  run ci` on PRs themselves, rather than only locally pre-merge. A related
  but distinct gap; not introduced here.
- Minor/major version bumps as anything other than a manual `Cargo.toml`
  edit inside a PR — no commit-message or label-based bump-type inference.
- macOS binary notarization (requires a paid Apple Developer ID).
- x86_64 binaries for either platform — nothing in this repo's existing
  tooling (`docker-build`, the dev machine) currently targets them.
- Pinning third-party GitHub Actions (`actions/checkout`, `jdx/mise-action`)
  by commit SHA instead of version tag — this repo has no existing
  precedent either way (first workflow ever); version tags are used for
  simplicity.

## Verification

- `mise run ci` clean on the branch.
- `mise run release -- --dry-run` locally: confirm it prints the expected
  `NEW_VERSION=`, correctly bumps `Cargo.toml`/cuts `CHANGELOG.md` on
  disk, runs `cargo publish --dry-run --allow-dirty` successfully, then
  `git checkout -- Cargo.toml Cargo.lock CHANGELOG.md` to discard.
- After merging: watch the Actions run on `main`, confirm it passes `mise
  run ci`, publishes successfully, and pushes a `chore(release): v0.4.2`
  commit + `v0.4.2` tag without retriggering itself.
- Confirm the crates.io page shows `0.4.2` shortly after.
- Confirm the `build-binaries` matrix job runs on both native arm64
  runners and the `v0.4.2` GitHub Release ends up with both
  `pigeon-v0.4.2-aarch64-apple-darwin.tar.gz` and
  `pigeon-v0.4.2-aarch64-unknown-linux-gnu.tar.gz` attached; download and
  run each on its respective platform to confirm `pigeon --version`
  reports `0.4.2`.
