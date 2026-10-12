# ADR-0077: selectable file types, adaptable transcoding mapping, and zip pass-through for `pull-transform`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-27.
- **Status**: Accepted. Reversed by ADR-0131.
- **Amends**: ADR-0074, ADR-0075, ADR-0076.

## Context

`pigeon job run pull-transform` (ADR-0074/0075/0076) currently pulls,
expands, transcodes, dates, dedups, and uploads **every** object in the
source bucket, unconditionally, with a fixed extension-to-canonical-format
mapping (`MediaKind::canonical_extension()`/`media::recode()`: Image -> jpg,
Video -> mp4, Audio -> m4a) and unconditional zip expansion. Real usage
against buckets with 50-100GB zips (ADR-0076) surfaced three gaps:

1. **No file-type filtering.** A bucket full of arbitrary content (photos,
   videos, PDFs, random documents, `.zip`s) is pulled/transcoded/uploaded in
   full every time -- there's no way to say "just the photos and videos,
   skip the PDFs."
2. **No control over the transcoding targets.** The canonical formats
   (jpg/mp4/m4a) are hardcoded; there's no way to see what they are before a
   run or pick an alternative (e.g. mkv instead of mp4).
3. **No opt-out of zip expansion.** Every zip is always expanded and every
   member transcoded individually -- for some zips (already a convenient
   single archive, or simply not worth the transcode cost) the user would
   rather just upload the zip file itself, untouched.

This ADR closes all three gaps in one pass since they share the same
mechanism: a per-item classification/skip gate already exists
(`worker.rs::process_item`'s `classify_extension` call), and the exact same
gate that lets an item through to processing is what upload naturally
respects too, since `upload::pending_upload_tasks` only ever walks
`local_output`, and only *placed* files land there.

**Decisions confirmed with the user**: file-type selection is
**per-extension** (reusing the existing pending-summary table exactly, not a
coarser `FileKind`-category grouping); the transcoding mapping is adaptable
via **a small, vetted per-category menu** (not fully-custom ffmpeg args);
and none of these choices persist across runs -- **every run confirms/adapts
fresh** (flags exist so a repeat run can skip the prompts, but nothing is
written to `keyring.toml`).

## Decision

### 1. Per-extension file-type selection

New wizard step in `wizard.rs::dispatch_async`, right after
`print_type_summary(&plan.type_summary)` and the `plan.tasks.is_empty()`
early-return, before `UploadTargetInput`:

```rust
struct FileTypesInput<'a> {
    flag: Option<Vec<String>>,
    available: &'a [TypeSummary],
}
impl WizardInput for FileTypesInput<'_> {
    type Value = HashSet<String>;
    fn flag_value(&self) -> Option<Result<HashSet<String>, String>> { ... }
    fn prompt(&self) -> Result<HashSet<String>, String> {
        // dialoguer::MultiSelect over `available` (label: "{extension} ({count}, {size})"),
        // all pre-checked -- same precedent as email_sync::wizard's IdentitiesInput.
    }
    fn non_interactive_fallback(&self) -> Result<HashSet<String>, String> {
        Ok(available.iter().map(|s| s.extension.clone()).collect()) // "everything", today's behavior
    }
}
```

New flag on the `PullTransform` clap variant (`job/cli.rs`), same style as
`email_sync`'s `--identities`:
```rust
#[arg(long, value_delimiter = ',')]
file_types: Option<Vec<String>>,
```
(`--file-types jpg,mp4,pdf`; the `"(none)"` bucket for extensionless keys is
selectable via the literal string `none`.)

**Enforcement, once, at the source**: `process_item` (`worker.rs`) checks
`allowed_extensions.contains(&extension)` immediately after
`classify_extension`, *before* any download -- an excluded top-level object
is never even fetched. A new `ItemOutcome::Skipped(SkipReason::TypeExcluded)`
variant (parallel to `Failed`) is tallied into a new
`PullTransformSummary.skipped_type: usize`, printed in `wizard.rs`'s final
summary line. This same gate runs for zip-extracted members too (they flow
through the identical `process_item` path), so an excluded type is dropped
whether it's a top-level object or something found inside a zip -- closing
requirement #3 (upload) for free: `upload::pending_upload_tasks` only walks
files that `dedup::place_files` actually placed under `local_output`, and an
excluded file never reaches placement. **No changes needed to `upload.rs`
at all.**

**Checkpoint carve-out**: a *root* task skipped for type-exclusion must
**not** be appended to `.processed` -- otherwise a later run with a broader
`--file-types` would silently see it as already done. (A zip member skipped
for type-exclusion behaves exactly like today's existing per-member
*failure* case: it doesn't block the zip's own root-key checkpoint, since
member-level failures already don't today.)

### 2. Zip pass-through (upload as-is vs. expand+transform)

New wizard step, only shown when `plan.tasks` contains at least one object
whose extension is `zip`:

```rust
struct ZipHandlingInput<'a> {
    flag: Option<Vec<String>>,       // --expand-zips <key,key,...>
    zip_tasks: &'a [&'a PullTask],   // every pending zip, key + size
}
impl WizardInput for ZipHandlingInput<'_> {
    type Value = HashSet<String>;    // keys that should be EXPANDED; all others pass through
    fn prompt(&self) -> ... {
        // MultiSelect listing each zip's key + formatted size, all pre-checked
        // ("uncheck any zip you'd rather upload as-is, untouched").
    }
    fn non_interactive_fallback(&self) -> ... {
        Ok(zip_tasks.iter().map(|t| t.key.clone()).collect()) // "expand all", today's behavior
    }
}
```
`--expand-zips` mirrors `--file-types`'s comma-delimited style. Omitted +
non-interactive keeps today's exact behavior (expand everything); omitted +
TTY prompts with everything pre-checked, so hitting Enter also reproduces
today's behavior exactly.

**Enforcement**: in `process_item`, the existing `FileKind::Zip` branch is
gated -- `if expand_zip_keys.contains(&item.display_key)`, do today's
`archive::expand_to_dir` path; **else**, route the zip through the exact
same handling as `FileKind::Other` (`process_document_or_other`): no
expansion, no date logic, streaming SHA-256 hash, placed/uploaded verbatim
under `zip/` with its original name. This reuses existing code -- no new
processing function needed, just a different branch in the existing
`FileKind::Zip => ...` match arm.

### 3. Adaptable transcoding mapping (small vetted menu)

New types in `media.rs`, replacing the fixed `MediaKind::canonical_extension()`
match:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageFormat { Jpg, Png }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoFormat { Mp4, Mkv, Webm }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioFormat { M4a, Mp3, Flac }

pub(crate) struct TranscodeTargets { pub image: ImageFormat, pub video: VideoFormat, pub audio: AudioFormat }
impl Default for TranscodeTargets { /* Jpg, Mp4, M4a -- today's exact defaults */ }
```
Each enum gets `extension(self) -> &'static str`, `parse(&str) -> Result<Self, String>`
(flag/prompt parsing), `all() -> &'static [Self]` (for the `Select` prompt),
and a `Display` impl. `media::recode()` is extended to pick both extension
and ffmpeg args from `(MediaKind, TranscodeTargets)` instead of a bare
`MediaKind` match -- one new, pre-vetted arm per added format (e.g.
`VideoFormat::Mkv` reuses the existing libx264/aac args in an `.mkv`
container; `VideoFormat::Webm` uses libvpx-vp9/libopus; `AudioFormat::Mp3`
uses libmp3lame; `AudioFormat::Flac` drops the bitrate flag for lossless;
`ImageFormat::Png` drops the `-q:v` flag, which is JPEG-quality-specific).
The existing "already matches, just copy" shortcut (today hardcoded to
`.m4a`) generalizes to "source extension already equals the *chosen*
target's extension."

This ADR doesn't touch `classify_extension`'s extension-to-`FileKind` table
at all -- `heic`/`heif` (common iPhone capture formats) already classify as
`FileKind::Image` today and already recode to jpg by default (that's the
whole point of the existing pipeline: HEIC isn't universally viewable, jpg
is). They stay exactly that way here: included by default in the
per-extension file-type selection (step 1) like every other extension, and
routed through the same adaptable `ImageFormat` target as every other image
extension (default `Jpg`, selectable `Png`) -- no special-casing needed;
called out explicitly since it's the most common real-world case this
feature needs to keep working correctly.

Three new flags (`--image-format <jpg|png>`, `--video-format <mp4|mkv|webm>`,
`--audio-format <m4a|mp3|flac>`), each independently resolved via the
standard `WizardInput` three-way (flag always wins and is validated against
the enum's `parse`; TTY prompts; non-interactive falls back to `Default`).

**Single combined confirm gate**, to avoid nagging on every run: a
`WizardInput<Value = bool>` step (styled like the existing `ConfirmInput`)
that only prompts on a TTY *and only if none of the three format flags were
passed*: prints the resolved default mapping (`Image (photo/screenshot) ->
jpg`, `Video -> mp4`, `Audio -> m4a`) and asks *"Adapt this before running?
[y/N]"* (default No, so Enter reproduces today's exact mapping). If yes,
prompts a `Select` per category (current default pre-selected). If any of
the three flags *was* passed, this gate is skipped entirely and each
category resolves independently through its own flag/default as normal.

### 4. Threading

`PullTransformJob` (`mod.rs`) gains three new fields, resolved in
`wizard.rs::dispatch_async` at the same point `remote`/`encryptor` are
resolved (right after gather, before concurrency/confirm):
```rust
pub allowed_extensions: HashSet<String>,
pub expand_zip_keys: HashSet<String>,
pub transcode_targets: media::TranscodeTargets,
```
`Job::run` passes them into `worker::run_pull_transform_job`, which threads
`allowed_extensions`/`expand_zip_keys` into `process_item`'s per-item gate
and `transcode_targets` into `process_media`/`media::recode`.

## Consequences

- Every new flag is optional and defaults to today's exact behavior
  (all types, all zips expanded, jpg/mp4/m4a) -- no existing non-interactive
  invocation or test changes behavior unless it opts in.
- `PullTransformSummary` gains `skipped_type: usize`; the wizard's final
  summary line grows one more clause.
- `process_item`'s signature grows two `&HashSet<String>` params and a
  `&TranscodeTargets`; `process_media`/`media::recode` grow a
  `&TranscodeTargets` param. Mechanical but touches most of `worker.rs`.
- A file whose zip-handling or type-filter choice the user wants to
  *change* on a later run needs its `.processed` entry cleared first if it
  was already checkpointed -- same pre-existing pattern as redoing any
  already-processed file, not a new gap.

## Out of scope

- Persisting file-type/zip-handling/transcode choices across runs (e.g. on
  `BucketConfig`, mirroring ADR-0027's encryption-key default) -- confirmed
  with the user as a deliberate simplicity choice; revisit if re-confirming
  every run proves tedious in practice.
- Fully-custom target extensions/raw ffmpeg args beyond the vetted
  per-category menu -- confirmed with the user as out of scope for now.
- Per-zip-member type filtering finer than the single global
  `allowed_extensions` set (i.e. no "different filter per zip").
- Pagination/search UX for an unusually long zip-file `MultiSelect` list.
- Magic-byte content sniffing (ADR-0074's never-implemented idea) -- this
  ADR's filter still keys entirely off `classify_extension`'s extension
  string, same as today.

## Verification

1. `mise run ci`: new unit tests for the `process_item` type-exclusion gate
   (excluded extension -> `Skipped`, not `Failed`, not checkpointed -- both
   for a root task and for a zip-extracted member), the zip pass-through
   branch (a zip key absent from `expand_zip_keys` produces one opaque
   `zip`-extension `ProcessedFile`, no `archive::expand_to_dir` call), each
   format enum's `parse`/`extension`/`Display`, and the three-way
   `WizardInput` resolution for all three new input types plus the combined
   confirm gate (flag-present skips the gate; TTY without a flag prompts;
   non-interactive without a flag defaults silently).
2. Manual: run against a small real/synthetic bucket mixing several
   extensions and at least one zip; confirm `--file-types` restricts both
   top-level and in-zip output correctly, an `--expand-zips`-excluded zip
   lands in the output tree untouched, and an adapted `--video-format mkv`
   produces a playable `.mkv` with the expected codec.
