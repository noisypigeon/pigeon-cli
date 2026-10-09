# ADR-0109: multiple source buckets for `pigeon job run deduplicate`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-08.
- **Status**: Proposed.

## Context

`pigeon job run deduplicate` (ADR-0082, renamed by ADR-0096) takes exactly
one `--source-bucket <alias>`. The desired shape is to run it against
**multiple** source buckets in one invocation, downloading all of them into
one shared local staging tree and deduplicating across the **combined**
set — not per-bucket-isolated dedup, which `email_sync::IdentityContext`
already demonstrates a (deliberately different) pattern for — e.g.:

```
mise run pigeon-release job run deduplicate \
  --source-bucket source1 --source-bucket source2 \
  --destination-bucket destination --report-bucket reports \
  --local-output /mnt/data/a --concurrency 8 --upload-concurrency 16 --yes
```

`mise run pigeon-release` already exists (`.mise.toml`, ADR-0088) — no mise
changes needed.

Two CLI-surface questions had no single obviously-correct answer and were
resolved directly with the user rather than guessed at:

- **How to spell "more than one source bucket"**: this codebase already has
  two live precedents for a multi-value flag — comma-delimited
  (`--identities`/`--file-types`/`--expand-zips`, `Option<Vec<String>>` +
  `value_delimiter = ','`) and repeatable (the now-removed `reduce` job's
  `--force-valuable`/`--force-reproducible`, bare `Vec<String>`). The chosen
  shape is the latter, reusing the *existing* flag name: `--source-bucket`'s
  clap type changes from `Option<String>` to `Vec<String>`, so
  `--source-bucket a --source-bucket b` repeats the flag and a single
  `--source-bucket a` keeps working unchanged. No new flag name, and it
  matches the common repeatable-flag idiom elsewhere (docker `-v`, curl
  `-H`) more directly than a comma-joined string would.
- **Whether to also rename `--remote-output`**: `deduplicate` always
  uploads its deduped result to exactly one destination, so once
  `--source-bucket` could mean "one or many," the existing `--remote-output`
  name read ambiguously next to it. It is renamed to `--destination-bucket`
  (still `Option<String>`, single value only) — **for `deduplicate`
  only**. `--remote-output` is used identically by three other job types
  (`email-sync`, `email-pull`, `pull-transform`); they keep that name
  unchanged, a deliberate, stated inconsistency rather than a larger
  4-job rename bundled into this ADR.

Reading the current implementation
(`src/commands/job/deduplicate/{mod,manifest,worker,dedup,wizard}.rs`,
`src/commands/job/shared_wizard.rs`, `src/core/data.rs`) surfaced one real
correctness risk that has to be fixed as part of this change, not deferred:
the `.processed` checkpoint and the `ContentIndex`/`deduplicate-report.txt`
are keyed purely by object key string, with zero bucket-identity awareness.
Once two different source buckets can legitimately contain an object with
the identical key string, today's flat checkpoint would let one bucket's
checkpoint entry silently cause the other bucket's identically-keyed object
to be skipped on a resumed run — silent data loss, the same class of bug
ADR-0030/0034/0098/0099 each previously root-caused and fixed in this
codebase. The same collision risk turned out to apply to several in-memory
tracking structures `run_deduplicate_job` builds during a single run
(`tainted_roots`, `finished_root_keys`, the placement pass's `finished_keys`)
even before anything reaches disk, since those are also keyed by bare
object key.

## Decision

### 1. Command surface

`src/commands/job/cli.rs`, `JobType::Deduplicate`: `source_bucket` becomes
`Vec<String>` (was `Option<String>`); `remote_output` is renamed to
`destination_bucket` (type unchanged, `Option<String>`). Every other field
on `Deduplicate` is untouched. `src/commands/job/commands.rs`'s dispatch arm
rebinds the renamed field.

### 2. `SourceBucketsInput`: a new, `deduplicate`-local wizard input

Added to `src/commands/job/deduplicate/wizard.rs` — **not** to
`shared_wizard.rs`. The existing shared `SourceBucketInput`
(`Value = String`) stays untouched and keeps serving its other consumer,
`pull_transform::wizard` (`import::wizard` already keeps its own
independent single-bucket copy). Per this codebase's "duplicate until the
third consumer" precedent (ADR-0082's own Out-of-scope section; applied
again by ADR-0090/0096), a new deduplicate-only input is the right scope —
one consumer doesn't justify generalizing a shared type that another job
would then have to deal with multi-bucket semantics it doesn't want.

Modeled on `email_sync::wizard::IdentitiesInput`, the closest existing
"mandatory, no-safe-default, multi-value, alias-resolved" shape:
`flag_value()` resolves every alias up front, erroring on the first unknown
one (no partial success); `prompt()` is a `dialoguer::MultiSelect` over all
configured bucket-configs, requiring a non-empty selection;
`non_interactive_fallback()` hard-errors with the same message text
`SourceBucketInput` already used ("--source-bucket is required when not
running interactively"), so the existing non-interactive-failure CLI test
keeps passing unmodified. A small `resolve_bucket(alias, store)` helper
(alias → `(BucketConfig, secret)`) replaces the inlined single-bucket lookup
`dispatch_async` used to do, now mapped over the resolved alias list.

### 3. Thread bucket identity end-to-end

- `DeduplicateJob.source_bucket: BucketConfig, source_secret: String` →
  `source_buckets: Vec<(BucketConfig, String)>`.
- `DeduplicateTask`/`QueueItem`/`HashedFile` each gain a `bucket_alias:
  String` field, populated at task-seeding time and copied unchanged onto
  every zip-member descendant (a zip's members share its parent's bucket).
- `gather_pending` loops over every `(bucket_config, secret)` pair, lists
  each bucket, and tags every resulting task with that bucket's alias. The
  wizard's pre-run type-summary table stays extension-keyed only — one
  combined, cross-bucket table, not broken out per bucket, matching this
  job's "treat as one set" design.
- `process_item`/`run_deduplicate_job` take `&[(BucketConfig, String)]`,
  built into a `HashMap<String, (BucketConfig, String)>` keyed by alias once
  up front; each item's own `bucket_alias` resolves which bucket/secret to
  download from. `download::download_with_retry` itself needs no changes —
  it already operates per-call against whichever single bucket is passed.

### 4. `.processed` checkpoint: compound-keyed

Checkpoint lines change from `{key}` to `{bucket_alias}\t{key}`; the
in-memory shape becomes `HashSet<(String, String)>`. A line with no tab
(pre-ADR-0109) parses as `("", key)` — a legacy sentinel that a lookup
treats as matching *any* bucket for that key, so an old single-bucket
run's checkpoint stays resumable without needing to know which single
bucket it originally used. Same `split_once('\t')`-with-fallback discipline
ADR-0099 already established for `ContentIndex`, applied here instead.

This same compound-keying was also applied to `run_deduplicate_job`'s
in-memory `tainted_roots`/`finished_root_keys` sets and the placement
pass's `finished_keys` list (`dedup.rs`) — all were previously keyed by
bare object key only, which would have let one bucket's tainted/finished
root incorrectly taint or finish a different bucket's identically-keyed
root within a single run, even before anything reached the `.processed`
file on disk.

### 5. `ContentIndex` and `deduplicate-report.txt`: bucket provenance

`src/core/data.rs`'s `ContentIndexEntry`/on-disk format gains a 4th
tab-separated field, `source_bucket_alias`, with the same graceful fallback
for older 2-/3-field lines (defaults to `""`). New methods
`commit_with_key_and_bucket`/`check_with_original_key_and_bucket` are added
alongside (not replacing) `commit_with_key`/`check_with_original_key` —
only `deduplicate::dedup` calls the `_and_bucket` variants, so
`pull_transform`, `email_sync`, and `email_pull`'s existing `ContentIndex`
call sites and on-disk format are completely untouched. (The pre-existing
2-tuple `check_with_original_key` had no other caller once `deduplicate`
switched to the 3-tuple variant, so it was removed rather than left dead;
its tests now exercise `check_with_original_key_and_bucket` directly.)

`deduplicate-report.txt`'s header gains trailing columns
`duplicate_bucket_alias\tkept_bucket_alias`, appended after the existing
four (`duplicate_key\tcontent_hash\tkept_path\tkept_original_key`) — append,
don't reorder, so anything already parsing the first four columns
positionally keeps working.

### 6. What stays unchanged

- `src/commands/job/download.rs` — already per-call bucket-scoped.
- Raw scratch filenames (`next_scratch_path`'s global `AtomicU64` counter) —
  already collision-safe across concurrent multi-bucket downloads.
- `ContentIndex`'s hash-keyed dedup *decision* logic — already correct for
  cross-bucket dedup by construction; only its recorded metadata gains a
  field.
- `--report-bucket` / upload phase — one report bucket, one destination
  bucket, regardless of source count.
- `local_output/result/<extension>/<name>` placement tree shape.
- The shared `SourceBucketInput` and `pull_transform::wizard`.

## Consequences

- `deduplicate` goes from exactly one source bucket per run to one-or-more,
  with the result deduplicated as a single combined set rather than
  per-bucket. `--source-bucket a --source-bucket b` is the only new CLI
  surface; a single `--source-bucket a` invocation is unaffected.
- `--destination-bucket` replaces `--remote-output` for `deduplicate` only —
  a breaking rename for this one job type, with no migration shim (this
  codebase's standing precedent for CLI surface changes, e.g. ADR-0017).
  `email-sync`/`email-pull`/`pull-transform` keep `--remote-output`
  unchanged.
- Three on-disk formats specific to `deduplicate` gain new fields/columns,
  each backward-compatible on read with the pre-ADR-0109 shape: the
  `.processed` checkpoint (bucket-scoped key), the `.content-hashes`
  `ContentIndex` (4th field), and `deduplicate-report.txt` (two trailing
  columns). None of these changes affect `pull_transform`'s,
  `email_sync`'s, or `email_pull`'s own on-disk formats.
- Per-item metrics (`record_phase`'s `source_bucket` label in
  `worker.rs`/`dedup.rs`) now naturally reflect each item's *actual* source
  bucket instead of one job-wide value, since there is no longer a single
  job-wide bucket to label with — a direct, required consequence of the
  type change rather than optional extra scope.

## Out of scope

- **Generalizing `SourceBucketsInput` into `shared_wizard.rs`** — only one
  consumer today; per the "duplicate until the third consumer" precedent,
  stays local to `deduplicate/wizard.rs`.
- **Per-bucket breakdown in the wizard's pre-run type-summary table** —
  `TypeSummary` stays extension-keyed only, matching the "combined set"
  design intent; a bucket-broken-out view would cut against that framing.
- **Bucket provenance on `archive::ArchiveFailure`** (zip-open failures in
  the report's manual-followup section) — a real but separable gap once
  multiple buckets can produce similarly-keyed failing zips; left for a
  follow-up so this ADR's `dedup.rs`/report diff stays scoped to the
  merge-record path it's actually changing.
- **Per-bucket `--concurrency`/`--upload-concurrency` tuning** — the
  existing flat pools already work correctly across buckets (one shared
  queue, any item from any bucket); no evidence of a need for per-bucket
  tuning.
- **Renaming `--remote-output` on the other 3 job types that have it** —
  explicitly decided against for this ADR (see Context); `email-sync`,
  `email-pull`, and `pull-transform` are unaffected.
