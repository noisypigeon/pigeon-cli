# ADR-0161: align pigeon-cli with the hub-and-spoke model

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-19.
- **Status**: Accepted. Not yet implemented. Amends ADR-0029 and ADR-0031.
  Needs a companion amendment to LIB-0017 §5 and LIB-0001 §2 in
  `noisypigeon/library` (§7).

## Context

[LIB-0001](https://github.com/noisypigeon/library/blob/main/docs/adr/LIB-0001-composable-hub-and-spoke-repo-architecture.md)
organizes this family of repos as a hub and its spokes. `noisypigeon/library`
owns every reusable, versioned asset. A spoke holds content, thin callers and
its own infrastructure, and consumes the hub **by version, never by branch**
(invariants 1 and 2). LIB-0001 §2 called pigeon-cli "already conformant;
consumes nothing from the hub". [LIB-0017](https://github.com/noisypigeon/library/blob/main/docs/adr/LIB-0017-spokes-do-not-get-their-own-adrs.md)
§5 then exempted it from the rule that spokes hold no ADRs, and its
`## Out of scope` made "moving or re-prefixing pigeon-cli's ADRs" a permanent
boundary.

Neither statement describes the repo any more:

1. **The hub already versions a copy of this repo's dashboard.**
   `grafana-dashboards/pigeon-cli/overview.json` is released as
   `grafana-dashboards/pigeon-cli/vX.Y.Z`. Its README says "pigeon-cli's own
   local observability stack currently ships a separate copy of this JSON.
   Pointing it at a pinned version here instead is a later step." The two
   copies have since drifted. The hub's copy selects its datasource through a
   `${prometheus}` variable and has a node-exporter disk-utilization panel.
   `observability/grafana/provisioning/dashboards/pigeon-cli-overview.json` has
   neither.
2. **`.claude/skills/release-pr/` is a stale copy of the hub's skill.** It
   describes releasing `terraform/modules/` changes with `release:*` labels
   through `module-release.yml`. This repo has none of these: no Terraform
   modules (they left with ADR-0084), no labels, and no such workflow. The
   hub's copy has moved on (`template-release.yml`, zola-site, dashboards). Here
   it is dead weight that matches requests like "ship this to main" and then
   gives instructions that don't apply.
3. **This repo holds a workflow body and four scripts.** `release.yml`,
   `release.sh`, `cut-release.sh`, `build-release.sh` and `adr-issue.sh` are
   what invariant 1 says a spoke must not hold. Nothing about them is
   pigeon-cli-specific apart from a handful of literals: the crate name, the
   sparse-index path `pi/ge/pigeon-cli`, the binary name `pigeon`, the codesign
   identifier `dev.pigeon.cli`, and the target matrix.
4. **`docs/` holds two things that are no longer live.** `docs/report/` has one
   job-run analysis from the since-removed `pull-transform` era.
   `docs/VISION.md` predates most of the decisions that now define the
   product. Both are kept in git history.
5. **The ADR corpus has a generic prefix.** Every other domain in the family is
   prefixed (`LIB-`, and the closed `DEV-`/`COM-`/`RUN-`/`NPC-`). A bare
   `ADR-0084` is ambiguous across repos, and this corpus itself holds the
   ambiguity. It cites the old monorepo's ADR-0037 and ADR-0052 (11
   occurrences, in CHANGELOG.md and ADRs 0050, 0051, 0078 and 0084). Those are
   now `DEV-0037` and `DEV-0052` in library's archive, not records of this
   repo.

## Decision

Align pigeon-cli with the hub-and-spoke model. The exceptions are recorded in
§6, so they are deliberate rather than drift.

### 1. `docs/adr/` → `adr/`, and the `CLI-` prefix

- **Move.** `docs/adr/NNNN-<slug>.md` becomes `adr/CLI-NNNN-<slug>.md`, all
  128 records (this one included), with `git mv` so history follows. `adr/` at
  the repo root is the spoke layout LIB-0017 describes (garden, blog and runway
  all used `adr/`).
- **Retitle.** Each H1 `# ADR-NNNN: …` becomes `# CLI-NNNN: …`.
- **Rewrite every citation.** Every `ADR-NNNN` that means a record of this repo
  becomes `CLI-NNNN`, across the whole repo: ADR prose (~2,389 occurrences),
  `src/` comments and strings (~1,130), `tests/` (~45), `CHANGELOG.md` (~410),
  `CLAUDE.md` (~256), `README.md`, the skill and the scripts. Every
  `docs/adr/` path citation (~45 in ADRs, plus CLAUDE.md, README.md and the
  scripts) becomes `adr/`, with the filename's new `CLI-` prefix.
- **Foreign IDs are rewritten to their real domain, not to `CLI-`.** The 11
  citations of `ADR-0037`/`ADR-0052` become `DEV-0037`/`DEV-0052`. The rewrite
  goes occurrence by occurrence, as ADR-0036 and ADR-0086 did for their path
  rewrites, and not as a blind find-and-replace. It must also handle the
  compound forms (`ADR-0036/0037/0052`, ~151 lines with a `/NNNN` tail), where a
  continuation number can belong to a different domain from its head.
- **Commit and branch convention.** It follows the prefix:
  `type(cli-XXXX): summary` on a `cli-XXXX-<slug>` branch. This amends
  ADR-0029's `adr-XXXX` spelling. Existing commit messages are history and are
  not rewritten.
- **Numbering** continues at max+1 within `adr/`. The next record is CLI-0162.

This **reverses LIB-0017 §5's permanent boundary** on moving or re-prefixing
this corpus. For this corpus only, it also departs from the never-rewrite rule
in DEV-0160 and LIB-0017 §3. The reason is that the corpus is still live and
cited from code (`src/` cites it over a thousand times). A two-prefix reading
rule ("a bare `ADR-` here means `CLI-`") would therefore live in every future
reader's head indefinitely, and a one-time rewrite is cheaper. Citations of
pigeon-cli's `ADR-` IDs in *other* repos are not touched. They are read as
`CLI-`, the same way DEV-0160 has a bare `ADR-` read as `DEV-`.

### 2. Skills come from a library plugin marketplace

- Library becomes a Claude Code plugin marketplace
  (`.claude-plugin/marketplace.json`) with one plugin, `pigeon-cli`, holding
  `skills/analyze-job-run/SKILL.md`. It is versioned on its own hub release
  track (`claude-plugins/pigeon-cli/vX.Y.Z`), which keeps tag shape private to
  the hub per LIB-0001 invariant 3.
- This repo enables it through a committed `.claude/settings.json`:
  `extraKnownMarketplaces` points at `noisypigeon/library` **pinned to that
  tag** (invariant 2), and `enabledPlugins` enables `pigeon-cli`.
  `.claude/skills/` is removed.
- **`release-pr` is deleted, not hoisted.** The hub already owns the real one,
  and nothing in this repo matches it (§Context 2).
- `analyze-job-run`'s citations become `CLI-` (§1) as part of the hoist, so the
  hub's first released copy is already correct.

### 3. The release workflow and all four scripts move to the hub

- **A reusable `rust-crate-release.yml`** (`workflow_call`) in library holds
  both of today's jobs: release (fmt-check, lint, cut, publish, GitHub Release)
  and the build-binaries matrix (build, smoke test, package, upload asset).
  `release.sh`, `cut-release.sh`, `build-release.sh` and `adr-issue.sh` move
  to library's `.github/scripts/`. Both ride the hub's existing
  `workflows/vX.Y.Z` track, which `template-release.yml` already cuts for any
  change under `.github/workflows/*.yml` or `.github/scripts/`.
- **The literals become inputs**: crate name, sparse-index path, binary name,
  codesign identifier, and the target matrix. These are the only
  pigeon-cli-specific values in the scripts today.
- **The caller here keeps what cannot move.** That is the `push` trigger, the
  `chore(release): v` loop guard, `permissions`, `concurrency`, and passing
  `CARGO_REGISTRY_TOKEN`. `github.*` is the caller's context, so the checkout,
  commit, tag and push are all still this repo's (see §6). This is
  `spoke-root-changelog.yml`'s split.
- **Local mise tasks keep working.** `release`, `build-release` and
  `adr-issue` run the hub's scripts after an inline mise task materializes
  library at the pinned version into a gitignored `.library/`. This is the
  `fetch_script` pattern the other spokes use. It is inline in `.mise.toml`
  and not a script file, so the repo holds no scripts. `build`, `pigeon` and
  `pigeon-release` keep their two-line inline codesign step. It is task
  configuration, not a script, and fetching the hub to build a debug binary
  would be a bad trade.
- **The hub version is declared twice.** It appears in the `uses:
  noisypigeon/library/.github/workflows/rust-crate-release.yml@workflows/vX.Y.Z`
  literal, because GitHub requires `uses:` to be a literal, and as
  `LIBRARY_VERSION` in `.mise.toml`'s `[env]`. A bump changes both in one PR.
  This is a known cost, accepted rather than worked around.
- **`adr-issue.sh` gains a repo input and an ADR-path input**, so this repo
  keeps filing in `noisypigeon/pigeon-cli` (§6) while the script is shared. Its
  usage example's `docs/adr/0009-…` becomes `adr/CLI-0009-…`.

### 4. The observability stack moves to the hub and is versioned

- `observability/`, meaning `docker-compose.yml`, `prometheus/`, `promtail/`
  and `grafana/provisioning/`, moves to library's `observability/pigeon-cli/`.
  It gets a new release track, `observability/pigeon-cli/vX.Y.Z`, added as an
  arm of `template-release.yml` beside the `grafana-dashboards/<set>` arm.
- **One dashboard, not two.** The stack's provisioning loads
  `grafana-dashboards/pigeon-cli/overview.json` from the same library tree, and
  the local copy is deleted. That closes the drift in §Context 1, and the hub
  copy becomes the only one to edit. Its `${prometheus}` variable resolves to
  the local Prometheus datasource. Its node-exporter disk panel stays empty
  locally, because the local stack runs no node-exporter. That is acceptable:
  the panel exists for the Cockpit-hosted deployment.
- **This repo pins the stack.** `.mise.toml` declares
  `PIGEON_OBSERVABILITY_VERSION`. `observability-up`, `observability-down` and
  `observability-logs` fetch that tag into `.library/` and run
  `docker compose -f .library/observability/pigeon-cli/docker-compose.yml`
  from there. Every compose-relative mount keeps working because compose
  resolves mounts against the compose file's own directory.
- README's local observability section (ADR-0128) points at the mise tasks and
  the hub, and no longer at a local `observability/` directory.

### 5. Removals

- `docs/report/` and `docs/VISION.md` are deleted, and `docs/` with them. Git
  history keeps both.
- Live references are fixed: README.md's `docs/report/` bullet and anything in
  CLAUDE.md. The three historical ADR mentions of these paths are left as they
  are. They record what existed when they were written, and a dangling path in
  a dated record is history, not a bug.

### 6. Recorded exceptions

pigeon-cli remains a spoke that differs from the shape LIB-0001 defines, in
these ways only:

- **It keeps its own ADR corpus**, now `adr/CLI-`. LIB-0017 §5's reasoning
  still holds: these are decisions about this crate's code, not applications
  of a hub standard.
- **It files its out-of-scope issues in `noisypigeon/pigeon-cli`** (ADR-0031),
  not in library (LIB-0017 §6). The issues are about this code.
- **It writes its own tags.** LIB-0001 §4 says "a spoke never writes a tag",
  but a crate's `vX.Y.Z` tags must point at this repo's commits. Its release
  commit, `CHANGELOG.md` bump and crates.io publish also stay here (ADR-0117
  and ADR-0150). The hub provides the workflow and scripts, but the release
  act belongs to this repo.
- **It keeps its own `CHANGELOG.md` convention** (ADR-0029's `[Unreleased]`
  bullets, retitled by the release) and does not adopt
  `spoke-root-changelog.yml`.
- **It keeps its `Dockerfile` at the root.** It describes this crate's runtime
  image (ADR-0087 and ADR-0155), not a reusable asset.

### 7. Companion library change and order of work

- A library PR amends LIB-0017 §5 and LIB-0001 §2's pigeon-cli row. The
  amendment records that pigeon-cli now consumes hub assets, uses `adr/CLI-`
  (lifting §5's permanent boundary), and keeps §6's exceptions. Per LIB-0017's
  own consequence, "the library PR lands first, and the spoke PR cites it".
- Order:
  1. **Library PRs.** These add the plugin marketplace, the reusable workflow
     and the scripts, the observability stack and its release arm, and the
     LIB amendment. Each is released, so there are tags to pin.
  2. **pigeon-cli pure rename.** `docs/adr/` → `adr/CLI-`, plus the citation
     rewrite (§1). There are no behaviour changes. This keeps DEV-0110's "pure
     rename first, rewiring second" rule, since a diff that renames 128 files
     and rewires the release pipeline has two unrelated failure modes.
  3. **pigeon-cli rewiring.** The thin release caller, the
     `.mise.toml`/`.library/` fetch, `.claude/settings.json`, and deleting
     `.claude/skills/`, `.github/scripts/`, `observability/` and `docs/`.

## Consequences

- **A change to the release pipeline, the skill or the observability stack is
  a two-repo change.** The library PR lands and releases first, and a pigeon-cli
  PR bumps the pin. This includes `analyze-job-run`'s own "update this cheat
  sheet in the same PR" rule, which now means "in a library PR, then a pin
  bump". This is the cost the hub charges every spoke.
- **One dashboard.** The local stack and the Cockpit deployment render the
  same versioned JSON.
- **The rename touches almost every file once.** The rewrite PR is large but
  mechanical, and the `CLI-` grep (see Verification) makes it checkable.
  `git log --follow` and `git blame -C` still trace history through the move.
- **The crate's published contents are unaffected.** `Cargo.toml`'s `include`
  list (ADR-0086) names neither `docs/` nor `adr/`.
- **A release now depends on the hub being reachable at the pinned ref.** A
  broken hub release cannot break pigeon-cli until it is pinned, which is the
  point of pinning.

## Out of scope

- **Pinning the observability images.** Every compose service uses ([#134](https://github.com/noisypigeon/pigeon-cli/issues/134))
  `:latest`, so versioning the stack does not version what it runs. This is a
  genuine gap and is deferred.
- **Adopting `spoke-root-changelog.yml`.** A permanent boundary, per §6.
- **Rewriting other repos' citations of pigeon-cli `ADR-` IDs.** A permanent
  boundary, per LIB-0017 §3. Those are read as `CLI-`.
- **Running a node-exporter in the local stack** so the disk panel populates.
  It isn't needed for local diagnosis, so this is a permanent boundary.

## Verification

For the implementation PRs:

1. `mise run ci` passes after each pigeon-cli PR.
2. `git grep -nE '\bADR-[0-9]{4}'` returns nothing. `git grep -n 'DEV-00(37|52)'`
   returns exactly the 11 rewritten foreign citations. `git grep -n 'docs/adr'`
   returns nothing. Every relative Markdown link under `adr/` resolves.
3. `ls adr | wc -l` is 128 plus any records added since, and every filename
   matches `^CLI-[0-9]{4}-`.
4. The first `main` push after the rewiring runs the hub's
   `rust-crate-release.yml`. It cuts, tags and publishes as before, and all
   four binary assets attach to the GitHub Release.
5. `mise run release -- --help`, `mise run build-release` and
   `mise run adr-issue -- --help` work from a clean checkout, with `.library/`
   materialized at `LIBRARY_VERSION`.
6. `mise run observability-up` brings the stack up from `.library/`, and
   Grafana shows the hub's `overview.json` populated from a local job run.
7. A fresh Claude Code session in this repo lists `analyze-job-run` from the
   `pigeon-cli` plugin, and does not list `release-pr`.
