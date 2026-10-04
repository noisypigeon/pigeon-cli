# ADR-0096: rename `dedupe` → `deduplicate`; add `pigeon job run reduce`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-03.
- **Status**: Accepted.

## Context

Two related changes, requested together.

**1. Rename `pigeon job run dedupe` → `pigeon job run deduplicate`.**
Clearer verb-form naming, and sets up the second change below to read
naturally as the next stage in the same pipeline: `deduplicate` then
`reduce`.

**2. Add `pigeon job run reduce`, a new job that runs *after*
`deduplicate`.** It consumes `deduplicate`'s already-deduplicated, already
extension-organized output bucket (`<extension>/...`, per
`deduplicate::dedup::place_one`). For each top-level extension directory,
it decides whether the content is genuinely valuable (keep) or an
artifact / piece of media (a TV show, a movie, a software installer, a
disk image) that's easily reproduced from an external canonical source
(skip) — then forwards only the valuable directories to a mandatory
destination bucket, printing the per-directory forward/skip decision so
it can be verified before anything uploads.

**Why this isn't a repeat of ADR-0094's `sort` removal.** `sort` was
removed because `dedupe`'s own placement step already produced everything
`sort` did (extension-flattening) — running `sort` after `dedupe` was a
no-op second pass over already-correct data, confirmed by reading both
jobs' identical `place_one` logic. `reduce` makes a genuinely new
decision `deduplicate` never makes (content-value classification), and it
has its own real cost rationale distinct from mere pipelining: this
codebase has no bucket-to-bucket server-side copy primitive (confirmed —
`src/commands/keyring/bucket/client.rs` exposes only `list_objects`/
`download_object_to_file`/`upload_if_changed`, no `copy_object`), so every
job that forwards an object must download it locally and reupload it.
`reduce` exists specifically to *skip* that round-trip for the
directories it's about to discard — real, not incidental, savings against
metered S3 egress/ingress. Running `reduce`'s classification as a flag on
`deduplicate` instead would still pay the download cost for everything,
since `deduplicate`'s download phase happens before any classification
could run.

**Design questions that had no single obviously-correct answer, resolved
directly rather than guessed at:**

- **Classification mechanism**: a small curated, hardcoded extension
  table — the same shape as `pull_transform::media`'s `MediaKind`/
  format-menu pattern (ADR-0077) — not a config file, and not an
  always-interactive per-directory prompt. An extension not in the table
  defaults to **valuable**: under-forwarding risks real, silent data
  loss, while over-forwarding only costs a bit of bandwidth — the safe
  side to default toward. Two repeatable override flags
  (`--force-valuable <ext>`, `--force-reproducible <ext>`) let a user
  correct a misclassification without a code change.
- **What happens to a "reproducible" directory**: skip upload only.
  Nothing is ever deleted anywhere — bucket-to-bucket like `deduplicate`,
  source bucket and local staging both untouched either way.
- **The "verify which directories are being forwarded" requirement**:
  satisfied by always printing the full classification table (extension,
  file count, size, classification, forward/skip) as part of the
  existing pre-run summary step, the same place `deduplicate`'s own
  `TypeSummary` table prints today. `--yes` still skips the interactive
  confirm prompt that follows, matching house style (`ConfirmInput`) —
  but the table itself is never skipped, interactive or not.
- **Encryption**: never offered, matching `deduplicate`/former-`sort`'s
  stance (ADR-0082/0083) — `reduce` is a filter, not a privacy-sensitive
  transform.
- **Both buckets mandatory**: unlike `deduplicate` (optional upload),
  `reduce` requires both `--source-bucket` and `--remote-output` — it has
  no "local-only" mode, since forwarding to a destination bucket is its
  entire purpose.

Findings from reading the current codebase that shape the decision below:

- `extension_of(key: &str) -> String` is currently duplicated verbatim in
  `pull_transform/manifest.rs` and `dedupe/manifest.rs`. ADR-0094's own
  text already flagged this exact pending hoist ("`pull_transform::manifest`,
  `dedupe::manifest` remain") as the one case it deferred. `reduce` is the
  third real consumer.
- `SourceBucketInput<'a> { flag: Option<String>, store: &'a Store }` is
  also currently duplicated verbatim in `dedupe/wizard.rs` and
  `pull_transform/wizard.rs`, for the same "private module" reason
  ADR-0082 §0 already documented for other pieces. `reduce` is the third
  real consumer of this one too.
- `reduce`'s primary work (listing + plain streamed download, no hashing,
  no zip-expansion, no recoding) is I/O-bound, not CPU-bound — it needs a
  flat concurrency input, not `CpuConcurrencyInput`. ADR-0094 just deleted
  the old shared, flat `ConcurrencyInput` from `shared_wizard.rs` since
  `sort` was its only consumer. `reduce` becomes its only consumer again;
  per this codebase's "duplicate until the third consumer" rule, it gets
  its own local copy rather than reversing ADR-0094's removal for a
  single consumer.

## Decision

### 0. Preliminary hoists

- **`extension_of`**: moved into `src/core/data.rs` as `pub(crate) fn
  extension_of(key: &str) -> String` (identical logic — lowercase
  extension, `"(none)"` for extensionless/dotfiles). `pull_transform`,
  `deduplicate`, and `reduce` all call the shared copy; both private
  copies deleted.
- **`SourceBucketInput`**: moved into `shared_wizard.rs` (identical
  logic — flag wins outright; interactive `store.prompt_select_bucket()`;
  non-interactive fallback errors `"--source-bucket is required when not
  running interactively"`). `pull_transform`, `deduplicate`, and `reduce`
  all use the shared copy; both private copies deleted.

### 1. Rename `dedupe` → `deduplicate`

Mechanical, no behavior change beyond the name:

- `src/commands/job/dedupe/` → `src/commands/job/deduplicate/` (all 5
  files: `mod.rs`, `manifest.rs`, `dedup.rs`, `worker.rs`, `wizard.rs`).
- Identifiers: `DedupeJob`→`DeduplicateJob`, `DedupePlan`→`DeduplicatePlan`,
  `DedupeSummary`→`DeduplicateSummary`, `DedupeDedup`→`DeduplicateDedup`,
  `DedupeTask`→`DeduplicateTask`, `run_dedupe_job`→`run_deduplicate_job`.
  `run_upload_only`/`upload_only_preflight_ok` are unchanged — already
  job-agnostic names.
- `cli.rs`: `JobType::Dedupe { .. }` → `JobType::Deduplicate { .. }`;
  `command_name()` arm `"job.dedupe"` → `"job.deduplicate"` (clap derives
  the subcommand string from the variant name, so `pigeon job run
  deduplicate` falls out automatically).
- `commands.rs`/`mod.rs`: `use`/dispatch/`pub mod` updated to match.
- On-disk report filename: `dedupe-report.txt` → `deduplicate-report.txt`.
- The literal job-label string passed to `upload::pending_upload_tasks`
  (the `pigeon_job` metric/log label, currently `"dedupe"`) →
  `"deduplicate"`.
- `tests/cli.rs`: all 5 `dedupe`-named tests renamed (function names and
  embedded `"dedupe"` CLI-arg/assertion strings) to `deduplicate`.
- `src/observability/metrics.rs`: doc comment "dedupe's
  `download`/`hash`/`place`" → "deduplicate's `download`/`hash`/`place`".
- `.claude/skills/analyze-job-run/SKILL.md`: every `dedupe`/`job.dedupe`
  reference fully updated (command_name list, step vocabulary,
  completion-summary format string) — this documents *current* behavior,
  so it gets the same full-rename treatment ADR-0094 gave it for `sort`'s
  removal, not the historical-record treatment below.
- `docs/adr/0082-dedupe-job.md`: **Status line only** amended — `Accepted`
  → `Accepted. Renamed dedupe → deduplicate by ADR-0096 (module/identifier
  names in the body below are historical).` Body left untouched, same
  "amend one line, leave the rest as the historical record" pattern
  ADR-0094 used on `docs/adr/0083-sort-job.md`'s Status line.
- `docs/adr/0089`/`0090`/`0091`/`0095` and their `CLAUDE.md` bullets:
  **left untouched** — accurate records of what shipped at the time,
  same precedent ADR-0094 already established for not rewriting history
  it didn't originate.
- `CLAUDE.md`: ADR-0082's bullet gets a trailing rename note (mirroring
  the ADR-0083 bullet's existing "**Reversed by ADR-0094**" trailer); new
  bullet added for this ADR.
- `CHANGELOG.md`: one `[Unreleased]` bullet, added in a follow-up commit
  once the PR exists (CLAUDE.md Dev cycle step 5).

### 2. New job: `pigeon job run reduce`

New directory `src/commands/job/reduce/{mod,classify,manifest,worker,wizard}.rs`.

**`classify.rs`** — the new classification concept, structurally
imitating `pull_transform::media`'s closed-enum-plus-table shape:

```rust
pub(crate) enum ContentValue { Valuable, Reproducible }

// Movie/TV video containers + software/installer/disk-image artifacts --
// both "easily re-acquired from an external canonical source."
const REPRODUCIBLE_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "m4v", "ts", "mpg", "mpeg",
    "iso", "dmg", "exe", "msi", "pkg", "deb", "rpm", "appimage",
];

pub(crate) fn classify_extension(
    extension: &str,
    force_valuable: &[String],
    force_reproducible: &[String],
) -> ContentValue {
    if force_valuable.iter().any(|e| e == extension) {
        return ContentValue::Valuable;
    }
    if force_reproducible.iter().any(|e| e == extension) {
        return ContentValue::Reproducible;
    }
    if REPRODUCIBLE_EXTENSIONS.contains(&extension) {
        ContentValue::Reproducible
    } else {
        ContentValue::Valuable
    }
}
```

**`manifest.rs`** — `ReduceTask { key: String, size: u64 }`;
`ExtensionSummary { extension: String, count: usize, total_bytes: u64,
value: ContentValue }` (the printed verify-table's row type, covering
*every* extension found, forwarded or not); `PROCESSED_FILE_NAME`/
`load_checkpoint`/`append_checkpoint` (byte-for-byte the same shape as
`deduplicate/manifest.rs`'s, staging-dir-scoped); `gather_pending(...)` —
lists the source bucket (`client::list_objects`, recursive), groups by
the now-shared `core::data::extension_of`, classifies each extension once
via `classify::classify_extension`, and returns `ReduceTask`s only for
keys whose extension classified `Valuable` (a `Reproducible` extension's
objects are never even added to the download queue).

**`worker.rs`** — download-only phase, reusing `download.rs::check_disk_space`/
`download_with_retry`. No hashing, no zip-expansion, no `spawn_blocking`
— nothing here is CPU-bound. Places each forwarded file at
`result/<extension>/<sanitized-unique-name>`, the same shape as
`deduplicate::dedup::place_one` minus the `Dedup`/`ContentIndex` check
(`reduce` filters, it doesn't dedupe — its input is already unique).
`FailureBreakdown { download, placement }` (narrower than `deduplicate`'s
— no archive/hash phases). `ReduceSummary { forwarded,
skipped_low_value, failed, failure_breakdown, uploaded, unchanged,
upload_failed }`. `run_reduce_job(...)` entry point;
`upload_result`/`run_upload_only` mirror `deduplicate::worker`'s shape
exactly, including reuse of `upload::pending_upload_tasks`/
`run_upload_phase`'s existing `.staging/.uploaded` resume index.

**`wizard.rs`** — uses the now-shared `SourceBucketInput` (§0); a
mandatory, own-struct `RemoteOutputInput`-style input (alias of a
bucket-config, always required — no "upload to a bucket-config?" confirm
gate the way `deduplicate`'s optional `UploadTargetInput` has, since
`reduce` has no local-only mode); `LocalOutputInput` (own copy, same
shape as `deduplicate`'s); a flat, non-CPU-aware `ConcurrencyInput` (own
local copy per the Context section's reasoning above);
`UploadConcurrencyInput` (shared — inherits ADR-0091's lazy-hash-skip and
per-attempt timeout for free); `ConfirmInput` (shared). `dispatch`/
`dispatch_async` mirror `deduplicate::wizard`'s structure: resolve source
bucket → `job.gather()` → **always print the `ExtensionSummary` table**
→ resolve mandatory remote output → resolve concurrency inputs → confirm
→ `job.run(...)`. `--upload-only` gets the same
`upload_only_preflight_ok`-shaped preflight (`.staging/.processed` exists
and `result/` is non-empty) and `dispatch_upload_only` split that
`deduplicate` has.

**`mod.rs`** — `ReduceJob` struct + `impl core::job::Job` (`type Plan =
ReducePlan; type Summary = ReduceSummary;`), same shape as
`DeduplicateJob`.

**CLI wiring**: `cli.rs` gains `JobType::Reduce { source_bucket:
Option<String>, local_output: Option<PathBuf>, remote_output:
Option<String>, concurrency: Option<usize>, upload_concurrency:
Option<usize>, upload_only: bool, force_valuable: Vec<String>,
force_reproducible: Vec<String>, yes: bool }`, doc comments matching the
wording conventions already established for `Deduplicate`/`PullTransform`'s
equivalent fields; `command_name()` arm → `"job.reduce"`. `commands.rs`
gains the `use`/dispatch wiring; `mod.rs` gains `pub mod reduce;`.

`tests/cli.rs` gains 6 tests mirroring `deduplicate`'s set:
`job_run_help_lists_reduce`,
`job_run_reduce_help_shows_source_bucket_and_concurrency_flags`
(asserting `--encryption-key`/`--file-types`/`--expand-zips` absence,
same as `deduplicate`'s), `job_run_reduce_upload_only_without_a_completed_run_fails_fast`,
`job_run_reduce_without_source_bucket_fails_fast_non_interactively`,
`job_run_reduce_with_unknown_source_bucket_fails_fast`, and
`job_run_reduce_without_remote_output_fails_fast_non_interactively` (new
— specific to `reduce`'s mandatory-output shape, which `deduplicate`
doesn't have since its upload is optional).

`.claude/skills/analyze-job-run/SKILL.md` gains `job.reduce` in the
`command_name()` list, a `reduce: download, placement (also upload)` step-
vocabulary bullet, and its completion-summary format string.

## Consequences

- `pigeon` goes from 4 upload-capable job types to 5 again
  (`email-sync`/`email-pull`/`pull-transform`/`deduplicate`/`reduce`).
- The `pigeon_job` metric/log label `"dedupe"` becomes `"deduplicate"` —
  any existing Grafana/Cockpit dashboard or saved query filtering on
  `pigeon_job="dedupe"` (ADR-0092/0093, deployed via the sibling
  `noisypigeon` repo's `modules/scaleway/compute-instance`) will need its
  filter updated to match; not fixed here, since that's a different
  repo's dashboard configuration, not this one's code.
- `reduce`'s classification table is a curated guess, not a guarantee —
  an extension landing in `REPRODUCIBLE_EXTENSIONS` that turns out to
  contain genuinely irreplaceable content (e.g. a home movie saved as
  `.mp4`) will be silently skipped unless the run used
  `--force-valuable mp4`. This is the accepted tradeoff of defaulting
  unknown/ambiguous content to a hardcoded per-extension table rather
  than inspecting actual file content — documented explicitly, not
  hidden.
- `reduce` reuses `deduplicate`'s on-disk conventions (`.staging/`,
  `result/<extension>/`, `.processed`, `--upload-only`) closely enough
  that anyone debugging one can read the other's code directly.

## Out of scope

- Any per-file (as opposed to per-extension-directory) classification —
  the user's own framing ("look at the contents of directories by file
  type") and the already-extension-organized input bucket both point at
  directory-level granularity; a permanent design boundary, not a
  deferred gap.
- Content-aware classification (actually opening a file to judge its
  value, e.g. OCR on a PDF or a perceptual check on a video) — the
  classification is extension-only, same granularity `pull_transform`'s
  `MediaKind` already uses for its own format menu.
- Retroactively re-running `reduce` against buckets `deduplicate` already
  produced before this ADR — a user-initiated choice, not something this
  change performs automatically.
- Generalizing `deduplicate`/`pull_transform`/`reduce`'s now-three-times-
  duplicated bucket-listing/`TypeSummary`-style manifest pattern beyond
  the two pieces (§0) already hoisted this round — `extension_of` and
  `SourceBucketInput` were clear, mechanical, zero-risk hoists; the
  larger manifest-module generalization is the pre-existing deferred item
  tracked at [noisypigeon/noisypigeon-2#97](https://github.com/noisypigeon/noisypigeon-2/issues/97)
  (per ADR-0083's own note, now with one more duplicate instance rather
  than fewer) — still not tackled here, on-disk migration risk to
  already-shipped jobs unchanged from when ADR-0083 first deferred it.

## Verification

- `mise run ci` clean (fmt, clippy, full test suite including the new
  `reduce` tests and renamed `deduplicate` tests).
- `pigeon job run --help` lists `deduplicate` and `reduce`, not `dedupe`.
- `pigeon job run dedupe --help` fails as an unrecognized subcommand.
- `pigeon job run reduce --help` shows `--source-bucket`,
  `--local-output`, `--remote-output`, `--concurrency`,
  `--upload-concurrency`, `--upload-only`, `--force-valuable`,
  `--force-reproducible`, `--yes`; does not show `--encryption-key`/
  `--file-types`/`--expand-zips`.
- Repo-wide grep for `"dedupe"` as a job-name/command-name string
  (excluding `docs/adr/0082/0089/0090/0091/0095`, `CLAUDE.md`,
  `CHANGELOG.md` historical prose, and unrelated `.sort()`-style calls)
  turns up nothing left to rename.
