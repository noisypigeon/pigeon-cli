# ADR-0139: one input file type, many extension spellings

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

A real run silently did nothing:

```
job run transform --input-file-type=jpeg --source 'source:jpg/' \
  --destination 'destination:jpg/' --report-bucket reports --non-interactive ...

Input file type: jpeg
Source:          source:jpg/
Destination:     destination:jpg/
Pulled 0 file(s); 0 transcoded, 0 copied through, 0 failed; pushed 0 file(s).
```

`source:jpg/` was full of `.jpg` files. The run exited `0`, and its report
recorded `0 rclone error(s)` -- nothing failed, because nothing was ever
attempted.

`InputFileType::parse` has accepted `jpg` as an alias for `jpeg` since
ADR-0112 §71. But `InputFileType::extension()` returned a **single**
canonical string, `"jpeg"`, and that one string became the source
listing's `rclone --include` pattern. Verified against the real binary:

```
rclone lsjson src --include '*.jpeg' --ignore-case  ->  c.jpeg        (a.jpg, b.JPG missed)
rclone lsjson src --include '*.jpg'   --ignore-case  ->  a.jpg, b.JPG
```

`--ignore-case` folds case but cannot make `jpeg` match `jpg`. So the
alias was deliberate at the *input-parsing* layer and never reached the
*file-matching* layer. ADR-0112 states both facts 13 lines apart (§71 the
alias, §84 the `--include '*.<ext>'` filter) without connecting them;
ADR-0122 §51 then cited the alias approvingly as precedent while
reaffirming "one-extension-per-run semantics". That constraint is what
this ADR revisits. ADR-0083 §53 is the only prior place in the repo that
confronted the `.jpg`/`.jpeg` split, for a different job, and accepted it
deliberately -- the connection to `transform`'s single-pattern filter was
never made.

Two independent gates failed the same way, not one:

1. **Source listing** (`worker.rs` → `destination::list_source_relative_paths`
   → `Lsjson { include_extension: Some("jpeg") }`). This listing is the
   sole input to the pull set, and an empty pull set makes
   `run_transform_job` skip spawning `rclone` altogether -- which is why
   the report showed zero errors alongside zero work. This is the reported
   failure.
2. **Local on-disk scan** (`manifest::gather_pending`), an exact-string
   compare, `actual_extension != Some(expected_extension)`, skipping
   non-matches with a `tracing::warn!`. A `.jpg` already staged locally
   was dropped here **even once gate 1 was fixed**. A second bug with the
   same root cause, found only by auditing every consumer rather than
   stopping at the first.

Three further findings from that audit:

- Every pre-existing jpeg test used a `.jpeg` fixture and **none** used
  `.jpg`, so a filter that matched exactly one spelling passed the entire
  suite. That is the coverage hole that let this ship.
- `Lsjson`/`Copy`'s `--include` argv construction had **no test at all**.
- `Copy.include_extension` was **dead**: ADR-0124 moved the pull to
  `--files-from-raw`, so the pull `Copy` stopped filtering by extension,
  but the field and its now-false doc comment remained.

## Decision

### 1. An input kind has a set of spellings and one canonical spelling

`InputFileType::extension()` is replaced (not wrapped) by three methods,
so no call site can keep silently matching one spelling:

```rust
/// Every on-disk spelling this kind matches.
fn extensions(self) -> &'static [&'static str]   // Jpeg => &["jpg", "jpeg"]

/// Display and default-bucket-prefix only -- never matching.
fn canonical_extension(self) -> &'static str     // extensions()[0]

/// Case-insensitive membership, replacing the exact-string compare.
fn matches_extension(self, ext: &str) -> bool
```

**Only `Jpeg` gains a second spelling.** `Png`, `Heic`, `Mov`, `M4v`, and
`Mp4` keep exactly one, enforced by a test. No speculative `heif`/`tif`
aliases: a new alias gets added when a real run is observed to miss
files, not in anticipation. A video kind deliberately does not match a
sibling video extension -- a run still transcodes exactly one input kind,
which is the half of ADR-0122 §51's constraint that survives.

### 2. Jpeg's canonical spelling becomes `jpg`

`Display` and ADR-0138's prefix inference both move from `jpeg` to `jpg`,
resolving a live asymmetry: `output_extension()` has always returned
`"jpg"` for `Jpeg`, so a `--input-file-type=jpeg` run defaulted its
*source* prefix to `jpeg/` but its *destination* prefix to `jpg/`. It also
matches ADR-0112's own documented example (`source:png/` →
`destination:jpg/`).

Confirmed with the operator before changing it: both `source:jpeg/` and
`source:jpg/` exist in the real bucket; `jpeg/` has already been processed
and `jpg/` has not. Matching accepts both spellings regardless, so an
explicit `--source 'source:jpeg/'` behaves exactly as before -- only the
*default* changes.

### 3. The listing filter accepts multiple extensions

`Lsjson.include_extension: Option<String>` becomes `include_extensions:
Vec<String>`, emitting one `--include '*.<ext>'` per entry plus a single
`--ignore-case` (one such flag governs every filter rule). rclone ORs
repeated `--include` flags; verified against the real binary, as was the
brace alternative `*.{jpg,jpeg}`. Repeated flags are chosen over smuggling
brace syntax through a field named for a single extension.

`destination::list_source_relative_paths` widens to `extensions: &[&str]`.
`list_existing_filenames` is unchanged -- it deliberately passes no
filter, since it must see every `.jpg` output name whatever the input kind
was.

The dead `Copy.include_extension` and its stale doc comment are deleted.

### 4. A zero-match source listing warns on both channels

A listing that matches nothing is byte-for-byte indistinguishable from a
successful no-op, which is why this bug was invisible. An empty listing
now emits:

```
WARNING: source:jpg/ matched 0 files for --input-file-type jpeg
  (patterns: *.jpg, *.jpeg). Nothing to do -- check the prefix and file type.
```

Deliberately **both** an `eprintln!` and a `tracing::warn!`:
`observability::init` registers no console layer by design (ADR-0073, so
`indicatif` progress bars never share stderr with a second writer), so a
`warn!` alone reaches only `pigeon.jsonl` and never the operator's
terminal -- exactly the gap that hid this. The `eprintln!` is safe here
specifically because `transform` runs no `MultiProgress` bars for a stray
write to corrupt (checked, not assumed).

The exit code stays **0**: an empty source is a legitimate no-op, and
failing would break a scheduled run over an intermittently-empty source.

## Consequences

- **`--input-file-type=jpeg` now processes `.jpg` files**, which is the
  reported fix. A previously-silent run over a `.jpg` tree will now do
  real work -- intended, but worth knowing before pointing it at a large
  bucket.
- **A jpeg run's default source prefix changes from `jpeg/` to `jpg/`.**
  Only affects runs that *omit* a prefix; an explicit `--source
  'source:jpeg/'` is unaffected. ADR-0138's prefix defaults are hours old,
  so the exposure is minimal.
- Because the destination name hashes `(source_path, relative_path)`,
  the same image reached via `source:jpeg/` and via `source:jpg/` yields
  two different destination objects. `transform` has never deduplicated
  (ADR-0112), so this is consistent with existing behavior, not new.
- A kind with multiple spellings means a single run can now pull two
  extensions at once. `output_extension()` is unchanged, so both still
  converge on one output extension -- `.jpg` and `.jpeg` inputs both
  produce `.jpg` outputs, copied through byte-for-byte.
- The local on-disk scan still filters by extension while the live-tail
  dispatch path (`worker::enqueue`) does not. Harmless -- the tail only
  ever sees files `rclone` already pulled against the filtered listing --
  and pre-existing. Named here rather than fixed.

## Out of scope

- Aliases for any kind other than `Jpeg` (`heif`, `tif`, …) -- added on
  evidence of a real miss, not in anticipation.
- Matching several *different* input kinds in one run (e.g. png and heic
  together). ADR-0122 §51's one-input-kind-per-run semantics stand; this
  ADR only makes one kind's own spellings plural.
- Deduplicating a destination reached via two different source prefixes --
  `transform` has never deduplicated.
- Unifying `gather_pending`'s extension filter with `enqueue`'s lack of
  one (see Consequences).

## Amends

ADR-0112 (its `jpg`-alias decision never reached file matching, and its
`--include '*.<ext>'` filter becomes plural), ADR-0122 (its
"one-extension-per-run" phrasing, now one-input-*kind*-per-run with
possibly several spellings), and ADR-0138 (jpeg's canonical default prefix
moves from `jpeg/` to `jpg/`). Each gets a short forward-pointing
"Amendment" section, per this repo's convention.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `format.rs`: `extensions()` per kind, Jpeg has both spellings, every
  other kind has exactly one, `canonical_extension()` is `jpg` for Jpeg
  and agrees with `output_extension()`, `matches_extension` is
  case-insensitive and rejects other formats.
- `command.rs`: **first-ever argv test for an `RcloneCommand`** -- two
  extensions emit two `--include` flags and exactly one `--ignore-case`;
  an unfiltered `Lsjson` emits neither.
- `destination.rs` (real `rclone` subprocess): a source holding
  `a.jpg`/`b.JPG`/`c.jpeg`/`d.png` returns exactly the first three for
  `&["jpg", "jpeg"]`.
- `manifest.rs`: `gather_pending` accepts `.jpg`, `.jpeg`, and `.JPG`
  under a `Jpeg` run and still skips a `.png`.
- `worker.rs`: **closes the coverage hole** -- an end-to-end
  `run_transform_job` over a source holding both `a.jpg` and `b.jpeg`
  (plus an excluded `c.png`) asserts both are pushed and copied through.
- Manual, the reported failure: rerun the operator's exact command
  against `source:jpg/` and confirm a non-zero copied-through count.

## Amendment (2026-10-13): four more single-spelling kinds (ADR-0149)

ADR-0149 adds `numbers`, `xlsx`, `xls`, and `csv` to the extension-set model
this ADR introduced. Each has exactly one spelling, per this ADR's rule that an
alias is added on evidence of a real miss rather than in anticipation --
`calamine` reads `xlsm`/`xlsb`/`ods` too, and they are deliberately left out
until a run is observed to want them. `*.xls` does not glob-match `foo.xlsx`, so
`Xls` and `Xlsx` stay disjoint at the `--include` filter with no special
handling, and `every_other_kind_has_exactly_one_spelling` is extended to cover
all four.

Also relevant: this ADR's zero-match source-listing warning is the only thing
standing between an operator and a silently-invisible `.numbers` saved as a
package *directory*, which uploads as many objects under `Foo.numbers/` and
matches no `*.numbers` pattern. ADR-0149 records that as a known limitation on
exactly this ADR's reasoning.
