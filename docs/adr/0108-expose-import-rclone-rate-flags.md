# ADR-0108: expose `import`'s rclone transfer/checker/rate flags as CLI flags

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-07.
- **Status**: Proposed.

## Context

ADR-0101 added `pigeon job run import` as a thin wrapper around `rclone
copy <source> <destination>` with a fixed, non-configurable set of
performance/retry flags -- explicitly out of scope: "Exposing rclone's
performance/retry flags as pigeon CLI flags." ADR-0106 later retuned three
of those flags after a 33-run log review found Backblaze B2
rate-limiting responsible for over 99.9% of ~494K sampled WARN/ERROR
lines: `--transfers` 32→8, `--checkers` 64→16, and a new `--tpslimit 10`
(previously absent -- rclone's own uncapped default). ADR-0106 explicitly
kept ADR-0101's "fixed, not CLI flags" stance, reasoning that every data
point implicated one provider (B2) and there was no evidence yet that a
different destination needed different numbers -- but it named the exact
condition that would overturn that stance: "if one fixed value can't
serve every provider `import` is used against."

That condition has now been met, on a different provider than the one
ADR-0106 tuned for. A real run against a 16GB bucket (legacy-named
`backblaze-google-consolidation` from when it lived on Backblaze B2, but
now actually hosted on **Scaleway**) ran for over an hour without
completing and had to be killed -- `import` runs on a billed cloud VM, so
an open-ended hang costs real money, not just wall-clock time. This is
not the failure mode ADR-0106 fixed (B2 429 floods exhausting rclone's
retry budget); it's the opposite problem, on a different backend: the
same fixed `8/16/10` values that stopped B2 from rate-limiting are too
conservative for a Scaleway-backed transfer of this size. The most
plausible mechanism is `--tpslimit 10`, which caps *all* checker and
transfer transactions combined at 10/second -- punishing for a bucket
with many small files, where each file costs multiple transactions
(list/stat/hash-compare plus the transfer itself) against that one
shared budget. ADR-0106's own mandated manual-verification rerun was
effectively attempted here, against a different provider than it was
tuned for, and failed to validate the chosen constants as universal.

## Decision

### 1. Add three optional CLI flags to `pigeon job run import`

`--transfers <N>`, `--checkers <N>`, and `--tpslimit <N>`, added to the
`Import` variant of `JobType` (`src/commands/job/cli.rs`) and threaded
through `src/commands/job/commands.rs`'s dispatch arm into
`import::wizard::dispatch`.

### 2. Two new `WizardInput`s with a flat, non-erroring default

`TransfersInput { flag: Option<usize> }` and
`CheckersInput { flag: Option<usize> }` (`src/commands/job/import/wizard.rs`,
kept local -- no second consumer yet), modeled directly on
`shared_wizard::UploadConcurrencyInput`'s existing shape: flag present →
use it; flag absent → prompt interactively (default pre-filled) or, when
not interactive, fall back to `Ok(default)` rather than erroring, since
this is a new flag being added to a command that already runs unattended
in scripts/cron today. Defaults stay at ADR-0106's tuned values --
`TransfersInput` defaults to `8`, `CheckersInput` to `16` -- unaffected by
this incident, just made overridable.

### 3. One new `WizardInput` with a different shape for the rate cap

`TpslimitInput { flag: Option<usize> }`, resolving to `Option<usize>`
rather than a plain `usize` -- "no cap" is itself a legitimate, distinct
value here, not just "use the baked-in default of 10":

- `flag_value`: `self.flag.map(|v| Ok(Some(v)))`.
- `non_interactive_fallback`: `Ok(None)` -- **no cap**, reverting
  ADR-0106's hardcoded `10` default.
- `prompt`: a string prompt (`Input::<String>::new().allow_empty(true)`)
  so a blank answer resolves to `None`; a non-blank answer parses to
  `Some(usize)` or re-prompts/errors on a bad number.

The default changes from ADR-0106's `10` to **unset/no cap** because a
single default rate ceiling has now been shown wrong in both directions
-- too loose for B2 (ADR-0106's own motivating incident), too tight for
Scaleway (this one) -- so the safer default is no cap at all, with a cap
available as an explicit per-run opt-in for a destination that actually
needs one.

### 4. Thread the three resolved values through to the rclone invocation

`ImportJob`/`ImportPlan` (`src/commands/job/import/mod.rs`) gain
`transfers: usize`, `checkers: usize`, `tpslimit: Option<usize>` fields,
carried from the wizard through `gather()` into
`worker::run_import_job` (`src/commands/job/import/worker.rs`), which
changes its fixed `.args([...])` array into a dynamically-built
`Vec<String>`: `--transfers {transfers}` and `--checkers {checkers}` are
always included (using the resolved value); `--tpslimit {n}` is included
only when `Some`, omitted entirely otherwise (so rclone runs with its own
uncapped default when no cap is set). Every other flag in that array --
`--fast-list`, `--buffer-size 32M`, `--multi-thread-streams 4`,
`--multi-thread-cutoff 256M`, `--retries 5`, `--low-level-retries 20`,
and the `--stats`/`--use-json-log`/logging flags -- stays a fixed
literal, unchanged.

### 5. Update doc comments asserting these flags are fixed

`run_import_job`'s own doc comment (`worker.rs`), the `Import` variant's
clap doc comment (`cli.rs`), and `ImportJob`'s module doc comment
(`import/mod.rs`) all currently describe these flags as fixed/not
configurable; each needs updating to describe the new override surface.

## Consequences

- An operator can raise `--transfers`/`--checkers` for a destination that
  tolerates more concurrency, or set (or leave unset) `--tpslimit` per
  destination provider, without a code change or a new pigeon release.
- Existing non-interactive invocations that predate this ADR keep
  `--transfers 8`/`--checkers 16` unchanged, but now get **no** rate cap
  by default instead of ADR-0106's `--tpslimit 10` -- a deliberate
  behavior change, called out explicitly here rather than landing
  silently. A destination that genuinely needs a B2-style rate ceiling
  must now opt in with `--tpslimit <N>`.
- `pigeon.jsonl`'s existing ADR-0106 log-collapsing behavior (consecutive
  identical-cause rclone errors collapsed into one "N more" summary) is
  unaffected -- it operates on whatever errors rclone actually reports,
  independent of which flags produced that traffic pattern.

## Out of scope

- The other fixed flags (`--retries`, `--low-level-retries`,
  `--buffer-size`, `--multi-thread-streams`, `--multi-thread-cutoff`,
  `--fast-list`) -- no operational evidence yet that these need per-run
  tuning; ADR-0101's broader anti-flag-sprawl rationale still applies to
  them.
- A `--rate-limit-profile`/named-provider-profile concept, floated as a
  documented-but-not-built contingency in ADR-0106 decision point 3 --
  superseded by this simpler direct-flag approach; not built.
- Auto-detecting or defaulting these values per destination provider
  (e.g. reading from `rclone.conf`'s remote type) -- out of scope; the
  operator supplies them explicitly per run.
- Retroactively re-running or remediating the killed Scaleway transfer --
  a follow-up manual action once this ADR lands, not part of it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- Unit tests on the three new `WizardInput`s mirroring existing
  `CpuConcurrencyInput`/`UploadConcurrencyInput` coverage: flag present
  uses it; flag absent and non-interactive falls back to the documented
  default (`8`/`16`/`None` respectively).
- Manual: re-run `job run import` against the Scaleway-backed bucket with
  an explicit `--transfers`/`--checkers` raise and no `--tpslimit`,
  confirming it completes in a reasonable time where the ADR-0106 defaults
  previously hung for over an hour.

## Amendment (2026-10-09): restructured into `rclone copy`/`rclone delete` (ADR-0110)

ADR-0110 renames this job `"import"` -> `"rclone-copy"`, nested under
`pigeon job run rclone copy`. The three flags this ADR exposed
(`--transfers`/`--checkers`/`--tpslimit`) are preserved unchanged on `copy`;
they are deliberately not offered on the new sibling `delete` action, which
has no file-transfer concurrency or rate concept to tune.
