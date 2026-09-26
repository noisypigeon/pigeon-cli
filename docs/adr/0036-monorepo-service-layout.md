# ADR-0036: restructure into a `service/` + `.github/` layout

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

This repo is being reshaped to prepare for a monorepo: `pigeon-cli`
becomes the first of possibly several services living under
`service/`, and repo-meta tooling that isn't part of the Rust package
itself moves under `.github/`. Concretely:

- `src/*` → `service/pigeon-cli/src/*`
- `tests/*` → `service/pigeon-cli/tests/*` (the repo has `tests/`,
  plural -- no `test/` directory exists today)
- `scripts/*` → `.github/scripts/*` (today just `scripts/adr-issue.sh`)

Every existing ADR's historical `src/`/`tests/`/`scripts/` path
citations get updated to the new locations as part of this restructure
-- a deliberate departure from how this repo handled every prior
rename (e.g. ADR-0009's `src/remote/` was never rewritten when
ADR-0017 later renamed it to `src/dataops/`; ADRs are normally treated
as a point-in-time record). Thoroughness was chosen over
historical-accuracy-as-written for this one, since a monorepo-wide
service split is a big enough structural break that leaving ~35
documents referencing a path that no longer exists at all was judged
more confusing than useful.

### What does and doesn't need to change

**Files that move** (via `git mv`, preserving history through Git's
rename detection):
- `src/` → `service/pigeon-cli/src/`
- `tests/` → `service/pigeon-cli/tests/`
- `scripts/` → `.github/scripts/` (a new `.github/` directory; none
  exists today)

**Files that stay at repo root, edited in place:**
- `Cargo.toml` -- today's `[[bin]] path = "src/main.rs"` becomes
  `path = "service/pigeon-cli/src/main.rs"`; `[lib]` (currently no
  explicit `path`, defaulting to `src/lib.rs`) needs an explicit
  `path = "service/pigeon-cli/src/lib.rs"` now that the default no
  longer resolves. Cargo's automatic integration-test discovery only
  looks in a `tests/` directory *adjacent to Cargo.toml* -- since
  Cargo.toml isn't moving, `tests/cli.rs` moving out from under it
  needs an explicit
  ```toml
  [[test]]
  name = "cli"
  path = "service/pigeon-cli/tests/cli.rs"
  ```
  block to stay wired up. `readme = "README.md"` is unaffected --
  README isn't moving.
- `.mise.toml` -- only one line references a moved path:
  `scripts/adr-issue.sh` → `.github/scripts/adr-issue.sh`. Every
  `cargo build`/`test`/`clippy`/`fmt` command is unaffected (they
  operate on whatever Cargo.toml is at the current directory, which
  isn't moving); `target/debug/pigeon`'s build-output path is
  unaffected too (Cargo's `target/` dir follows the workspace root,
  i.e. still repo root).
- `Cargo.lock` -- confirmed it contains no filesystem paths for the
  local `pigeon-cli` package (only name/version/dependency-graph
  data), so it needs **no manual edit at all** -- just a `cargo build`
  after the move to confirm it still resolves cleanly.
- `README.md` -- confirmed it has no `src/`/`tests/`/`scripts/`
  references (only `docs/adr/`, which isn't moving, and
  `cargo install pigeon-cli`, which is crates.io metadata, unaffected
  by local layout). **No changes needed.**
- `CLAUDE.md` -- the "## Commands" section only names `mise run <task>`
  invocations, no literal paths, so it needs no change there. Its
  per-ADR index bullets (e.g. ADR-0008's "`src/email/`") fall under the
  same historical-citation decision as the ADR bodies themselves --
  update them too, for consistency.

### The bulk historical rewrite is not a safe blind find-and-replace

A `grep` across `docs/adr/*.md` found 26 files with 193 total
`src/`/`tests/`/`scripts/` occurrences. Spot-checking surfaced a real
false positive before any edit was made: ADR-0022 contains the prose
*"Existing scripts/muscle memory need only insert..."* -- "scripts/
muscle memory" is idiomatic prose ("scripts or muscle memory"), not a
path reference. A naive `s/scripts\//.github\/scripts\//` would
corrupt it into "`.github/scripts/`muscle memory". Every occurrence
needs to be read in context before deciding whether it's a genuine
path citation (gets `service/pigeon-cli/` or `.github/` prepended) or
incidental prose (left untouched).

ADR-0031's *"the repo gains its first `scripts/` directory"* is a
borderline case worth naming explicitly: it's a historical claim about
a past milestone, not just a path -- the implementer should use
judgment (e.g. update the backtick-quoted path portion while leaving
the "first `scripts/` directory" narrative intact, since the directory
conceptually still exists, just nested now).

## Decision

1. `git mv src service/pigeon-cli/src`, `git mv tests
   service/pigeon-cli/tests`, `mkdir -p .github && git mv scripts
   .github/scripts`.
2. Update `Cargo.toml`'s `[lib]`/`[[bin]]` paths and add the `[[test]]`
   block, exactly as traced above.
3. Update `.mise.toml`'s `adr-issue` task's script path.
4. Add this ADR's own index bullet to `CLAUDE.md`.
5. Rewrite historical `src/`/`tests/`/`scripts/` path citations across
   every existing ADR in `docs/adr/*.md` (and CLAUDE.md's own per-ADR
   summary bullets) to their `service/pigeon-cli/`/`.github/`
   equivalents -- reviewed occurrence-by-occurrence, not scripted
   blindly, per the false-positive finding above.
6. No Cargo *workspace* conversion -- this ADR relocates the single
   existing package; it doesn't add a `[workspace]` manifest or a
   second member. If/when a second service actually gets added under
   `service/`, that's its own future ADR's decision (workspace vs.
   independent Cargo projects), not designed for speculatively here.

## Consequences

- `git log --follow`/`git blame` on any moved file need `--follow` (or
  the equivalent) to trace history across the move -- standard Git
  behavior for a rename, not a data-loss concern, but worth calling
  out so nobody's surprised by a plain `git log` stopping at the move
  commit.
- `cargo publish --dry-run` (ADR-0018's established publish workflow)
  should be re-verified after the move -- relocating `[lib]`/`[[bin]]`
  source paths is routine, but publishing is exactly the kind of thing
  worth confirming still works rather than assuming.
- The historical-ADR rewrite is a large, judgment-requiring diff
  (~193 occurrences, several false positives already found) -- a real
  cost of choosing "update everything" over "leave as historical
  record," worth stating plainly rather than treating as a mechanical,
  no-risk find-and-replace.
- Every ADR's own file:line citations need re-verification against
  current content before this bulk edit, since several ADRs have had
  small drift since they were written (line numbers shift as code
  changes) -- the rewrite should target the referenced *paths*, not
  assume every cited line number is still accurate (a pre-existing
  condition, not newly introduced by this restructure).
- Once `service/` exists, this repo's shape reads as "prepared for a
  monorepo" even though `pigeon-cli` is still the only service in it --
  the directory split is speculative in that sense, but scoped
  narrowly (a pure relocation, no workspace machinery, no new
  abstractions) rather than over-building for services that don't
  exist yet.

## Out of scope

- Converting to an actual multi-member Cargo workspace -- deferred
  until/unless a second service is genuinely added under `service/`.
- Any change to `docs/`, `CHANGELOG.md`, `LICENSE`, or `.gitignore`'s
  own locations -- all stay at repo root, unaffected.
- Renaming or restructuring anything *inside* `src/`'s existing module
  layout (`commands/`, `core/`, etc.) -- purely a relocation of the
  whole tree, no internal reorganization.

Implementation (the actual `git mv`s, `Cargo.toml`/`.mise.toml` edits,
and the historical-citation rewrite) is a separate, later task.

## Amendment (2026-09-25): also relocate `CHANGELOG.md`

### Context

While planning implementation, a further requirement came in:
`CHANGELOG.md` should move to `service/pigeon-cli/CHANGELOG.md` too.
This directly contradicts the Out of scope bullet above ("Any change
to `docs/`, `CHANGELOG.md`, `LICENSE`, or `.gitignore`'s own locations
-- all stay at repo root, unaffected"), so per this repo's own rule
(CLAUDE.md: flag a contradiction rather than silently diverging) this
amendment supersedes that bullet rather than editing it away silently.

Checked what actually references `CHANGELOG.md`'s location before
deciding this was safe to fold into the same restructure:
- `CLAUDE.md`'s "Dev cycle" section is the only *living* doc
  instructing where to write changelog entries (both its intro
  mention and the "Changelog" step's explicit path) -- both need
  updating to stay accurate.
- `docs/adr/0029-dev-cycle-pipeline.md` (the ADR that introduced
  `CHANGELOG.md`) cites it twice, historically -- falls under this
  ADR's already-established "rewrite historical citations" policy,
  just for a path pattern outside the original `src/`/`tests/`/
  `scripts/` grep.
- `Cargo.toml` has no `changelog` field (no such standard Cargo key
  exists; only `readme = "README.md"`, which isn't moving) and no
  `.mise.toml` task references `CHANGELOG.md`'s path -- confirmed
  nothing else touches this.

### Decision

`CHANGELOG.md` moves to `service/pigeon-cli/CHANGELOG.md` alongside
`src/`/`tests/`/`scripts/`, via `git mv`, in the same implementation
pass. The Out of scope bullet naming `CHANGELOG.md` above is
superseded by this amendment -- `docs/`, `LICENSE`, and `.gitignore`
remain unaffected and stay at repo root, as originally decided.
`CLAUDE.md`'s Dev cycle section and ADR-0029's historical citations of
`CHANGELOG.md` get updated to match, alongside the already-planned
`src/`/`tests/`/`scripts/` rewrite.

### Consequences

- Every future PR's changelog step writes to
  `service/pigeon-cli/CHANGELOG.md`, not the repo root -- CLAUDE.md's
  Dev cycle instructions are the load-bearing reference for this going
  forward.
- No other tooling or Cargo metadata depends on `CHANGELOG.md`'s path,
  so this addition doesn't expand the mechanical-move risk surface
  meaningfully beyond what was already planned for `src/`/`tests/`/
  `scripts/`.

Implementation of this amendment lands together with the rest of this
ADR's implementation, not as a separate task.
