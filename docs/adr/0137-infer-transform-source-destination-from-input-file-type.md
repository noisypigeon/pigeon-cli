# ADR-0137: infer `transform`'s `--source`/`--destination` bucket alias from `--input-file-type`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

ADR-0136 made `pigeon job run transform`'s `--source-bucket` and
`--destination-bucket` mandatory, keyring-validated `BucketConfig` aliases
(erroring on an unknown one, resolved to an rclone address via
`remote_address()`). Separately, and unrelated to that ADR,
`--input-file-type` already mechanically determines the output format:
`InputFileType::output_extension()` (`src/commands/job/transform/format.rs`)
maps every image kind (`png`/`jpeg`/`heic`) to `"jpg"` and every video kind
(`mov`/`m4v`/`mp4`) to `"mp4"`, with no separate `--output-file-type` flag
at all.

In practice, every real `transform` run addresses a source bucket-config
named after the content it holds and a destination bucket-config named
after the content it will receive -- e.g. a bucket-config aliased `heic`
holding HEIC input, one aliased `jpg` holding the transcoded output.
Typing both bucket flags on every invocation is pure repetition once that
naming convention is adopted, since `--input-file-type` already carries
the information needed to pick both aliases by convention.

This is deliberately scoped to `transform` only. `rclone copy`/`delete`
and `deduplicate` keep their own `--source-bucket`/`--destination-bucket`
exactly as ADR-0136 left them -- they have no file-type concept at all, so
there is nothing for them to infer a default from.

## Decision

### 1. Rename, `transform` only

`--source-bucket`/`--destination-bucket` become `--source`/`--destination`
on `transform`'s CLI surface. Breaking, no migration shim, consistent with
this codebase's established precedent (ADR-0017, ADR-0094, ADR-0096,
ADR-0103, ADR-0110, ADR-0114, ADR-0136). `rclone copy`/`delete`/
`deduplicate`'s own `--source-bucket`/`--destination-bucket` flags are
untouched by this ADR.

### 2. Both become optional, with an inferred default

When omitted, the default bucket-config alias is:

- source: `input_file_type.extension()` (e.g. `heic` for
  `--input-file-type heic`)
- destination: `input_file_type.output_extension()` (e.g. `jpg`)

This is a documented bucket-*naming* convention, not auto-detected content
inspection -- no bucket is ever listed or scanned to confirm it actually
holds that file type. The inferred alias is a pure function of the
already-resolved `--input-file-type` value, reusing the two `InputFileType`
methods that already existed for computing the output extension.

### 3. An explicit flag always wins

When `--source`/`--destination` is passed explicitly, it is validated
against the keyring exactly as before (`Store::contains_alias`, erroring
on an unknown alias) and used verbatim -- inference is skipped entirely in
that case, never blended with or treated as a fallback behind the inferred
value.

### 4. Resolution mechanics

`SourceBucketInput`/`DestinationBucketInput`
(`src/commands/job/transform/wizard.rs`) each gain a `default_alias: &str`
field, computed in `dispatch_async` from the already-resolved
`input_file_type` immediately before these two inputs are constructed
(`input_file_type` itself resolves, and hard-errors on an invalid or
missing value, strictly earlier in the same function -- this ordering
already existed and is unchanged):

- `flag_value()` is untouched, byte-identical to today. This is what
  guarantees an explicit flag always wins: inference logic is never
  consulted when a flag value is present.
- `non_interactive_fallback()` changes from an unconditional `Err` to: if
  the inferred alias is a configured bucket-config, `Ok(inferred)`;
  otherwise `Err`, naming the specific alias it looked for -- strictly
  more actionable than the old blanket "`--source-bucket` is required"
  message, since the operator is told exactly which bucket-config to
  create (or to pass the flag explicitly).
- `prompt()` still calls `Store::prompt_select_bucket`, now passing the
  inferred alias through so the interactive picker's own highlighted
  default row points at it when that alias is configured, instead of
  always defaulting to index 0. No separate announcement line is printed --
  the `Select` widget's own highlighted row, plus the existing
  post-resolution `println!("Source: ...")`/`println!("Destination: ...")`
  echo already in `dispatch_async`, are sufficient. This matches this
  codebase's existing convention for a computed (non-zero) `Select`
  default: `keyring/wizard.rs`'s `modify_email` defaults its provider
  picker to the identity's current provider with no separate announcement
  either.

### 5. `Store::prompt_select_bucket` gains a default-alias hint

`Store::prompt_select_bucket` (`src/commands/keyring/store.rs`) gains one
new parameter, `default_alias: Option<&str>`. Its 2-or-more-bucket-config
`Select` branch computes its default index via
`.position(|bc| bc.alias == alias).unwrap_or(0)` -- the same shape
`keyring/wizard.rs`'s `modify_email` already uses to default a `Select` to
a computed value rather than index 0. The 0-config (error) and
exactly-1-config (auto-return, no prompt) branches are untouched; neither
ever consults `default_alias`. This index computation is extracted into a
small free function, `default_bucket_index`, independently unit-testable
without a TTY -- `Select::interact()` itself needs a real terminal and
can't be driven from `cargo test`, and before this change the
2-or-more-config branch had zero test coverage of any kind.

Every other caller of `prompt_select_bucket` -- `rclone::wizard`'s own
`SourceBucketInput`/`DestinationBucketInput` and
`shared_wizard::ReportBucketInput`/`UploadTargetInput` -- passes `None`,
preserving today's always-index-0 behavior exactly. No flag, behavior, or
default changes to `rclone copy`/`delete`/`deduplicate`.

## Consequences

- **Breaking CLI change, no migration shim**, on `transform` only:
  `--source-bucket`/`--destination-bucket` become `--source`/
  `--destination`. Any existing non-interactive script or cron invocation
  passing the old flag names breaks on the rename alone, independent of
  whether it would have benefited from inference.
- A bucket-config not named after the file-type-naming convention now
  requires the explicit flag where it always did before -- this changes
  nothing for anyone not relying on the new inference, since the old
  flags were always mandatory and explicit.
- `Store::prompt_select_bucket`'s signature changes for all 7 existing
  call sites (2 in `transform::wizard`, 2 in `rclone::wizard`, 2 in
  `shared_wizard`, plus 2 unit tests) -- each adds one `None`/
  `Some(alias)` argument; no behavior changes for the 5 non-`transform`
  call sites.
- `prompt_select_bucket`'s 2-or-more-bucket-config `Select` branch gains
  its first real unit-test coverage (via the extracted
  `default_bucket_index` function) -- previously entirely untested.
- `TransformJob`'s internal `source_path`/`destination_path` field names
  are unchanged -- they already held an rclone remote address regardless
  of CLI flag naming, and renaming them carries no behavior change, so
  it's left as pre-existing residue rather than churned here.

## Out of scope

- Any inference for `rclone copy`/`delete`/`deduplicate`'s own
  `--source-bucket`/`--destination-bucket` -- they have no file-type
  concept to infer a default alias from; untouched by this ADR.
- Auto-detecting a bucket-config's actual contents (listing or sampling
  its objects to guess a file type) -- the inferred alias is a pure
  function of `--input-file-type`'s already-resolved value, never
  inspected against real bucket contents.
- Any change to `--report-bucket` on any job -- untouched.
- A generic, cross-job "a bucket-alias flag can infer a default"
  mechanism in `shared_wizard.rs` -- kept local to `transform::wizard`,
  per this codebase's "duplicate until the third consumer" precedent,
  since there is no second consumer of this specific inference logic to
  justify hoisting it.

## Amends

ADR-0136 (keeps its "validated `BucketConfig` alias, no sub-path
addressing" decision entirely intact; only changes `transform`'s
`--source-bucket`/`--destination-bucket` from always-mandatory to
optional-with-inference). Gets a short forward-pointing "Amendment"
section, per this repo's established convention for amending a merged ADR
without silently rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `src/commands/keyring/store.rs`: new `default_bucket_index` unit tests
  (a matching alias returns its index; no match, or `None`, falls back to
  `0`); a smoke test confirming `prompt_select_bucket`'s 2-or-more-config
  branch doesn't panic without a TTY.
- `src/commands/job/transform/wizard.rs`: new `SourceBucketInput`/
  `DestinationBucketInput` tests covering inferred-fallback-succeeds,
  inferred-fallback-errors-naming-the-alias, and
  explicit-flag-overrides-inference-even-when-different.
- `tests/cli.rs`: `transform --help` shows `--source`/`--destination`, not
  `--source-bucket`/`--destination-bucket`; omitting both with a matching
  bucket-config configured proceeds past flag resolution; omitting both
  with no matching bucket-config fails fast, naming the inferred alias; an
  explicit flag overrides inference even when a differently-named inferred
  alias is also configured; an explicit-but-unknown alias still errors via
  `Store::contains_alias` exactly as before.
