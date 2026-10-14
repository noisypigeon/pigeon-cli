# ADR-0149: spreadsheet input types, and `transform`'s first one-input-many-outputs kind

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-13.
- **Status**: Proposed.

## Context

`job run transform` normalizes images to `.jpg` (ADR-0112), video to H.265
`.mp4` (ADR-0122), and audio-only `.mp4` to `.opus` (ADR-0142). A real
archive of spreadsheets -- `.numbers`, `.xlsx`, `.xls`, `.csv` -- is
reachable by no `pigeon` job at all. The wanted shape is the existing one:

```
job run transform --input-file-type xlsx --source source --destination destination
  -- resolves to source:xlsx/ -> destination:csv/ by ADR-0138/0139 inference
```

with `.csv` as the single output format for all four input kinds.

**This is not a fifth arm added to an existing pattern.** A Numbers or Excel
workbook holds *several* sheets, and CSV has no concept of more than one, so
one input file must produce several output files. Every input kind `transform`
has ever had produces exactly one, and that assumption is load-bearing in five
independent places -- three of which run *before any bytes are downloaded*:

| Where | What assumes one output |
|---|---|
| `placement::compute_destination_name` | a pure function returning **one** filename |
| `worker::files_needing_pull` | "already done?" = that one name present at the destination; decides the pull set (ADR-0124) |
| `worker::enqueue` | the same single-name check, as the post-pull backstop (ADR-0120) |
| `worker::process_one` | one scratch path, one `place_one`, one `push_one`, `ProcessResult = Result<PathBuf, _>` |
| `worker::FileOutcome` / `write_report` | `destination_filename: Option<String>` -- one name per report row |

The hard one is the pull filter. ADR-0124's whole mechanism rests on
`compute_destination_name` being a pure *forward* function of
`(source_path, relative_path)` needing no file content, so the destination
listing answers "already done" before the pull starts. **A workbook's sheet
count is not knowable without opening the file**, so the set of names an input
maps to is not a pure function of its path. §3 is the answer, and it is this
ADR's load-bearing decision.

Two smaller facts shape the rest. `transform` has **no local checkpoint** --
ADR-0120 removed it, because local disk never survives the ephemeral VM this
job runs on, leaving the destination listing as the sole authority. And
`ffmpeg` cannot read a spreadsheet, so this is also the first input kind
needing a different conversion tool than the one `check_ffmpeg_available`
hardcodes.

## Decision

### 1. Four new `InputFileType` variants

`Numbers`, `Xlsx`, `Xls`, `Csv` in `src/commands/job/transform/format.rs`:

- `extensions()` -> `&["numbers"]`, `&["xlsx"]`, `&["xls"]`, `&["csv"]`. One
  spelling each, per ADR-0139's deliberately unspeculative rule: no
  `xlsm`/`xlsb`/`ods` aliases added in anticipation of a miss nobody has
  observed. `*.xls` does not glob-match `foo.xlsx`, so `Xls` and `Xlsx` stay
  disjoint at the rclone filter with no special handling.
- `canonical_extension()` is inherited from `extensions()[0]`, and `name()`
  falls through to it -- these four are not ADR-0142's special case.
- `output_extension()` -> `"csv"` for all four. ADR-0142 made this an
  exhaustive `match` precisely so a new kind cannot silently inherit `jpg`;
  this is the first ADR to benefit.
- `is_video()` -> `false`, which correctly suppresses the `--video-quality`
  prompt (`dispatch_async` gates it on `is_video()`).
- A new `is_spreadsheet()` -> `true` for all four. This gates §4's pipeline
  and §6's preflight by *asking what a kind is* rather than by widening a
  `match` arm list at each site, which is what let the ADR-0141 audio-only
  dispatch bug hide in one.

Prefix inference needs no new code: `with_default_prefix`
(`src/core/rclone/config.rs`) already takes `canonical_extension()` for the
source and `output_extension()` for the destination, so `--input-file-type
xlsx` resolves `source:xlsx/` -> `destination:csv/`. All four kinds share one
`destination:csv/` prefix. Unlike ADR-0142's `.opus`-into-`mp4/` hazard this is
*correct* rather than merely tolerated: the four are the same output format,
and `compute_destination_name` hashes `source_path`, so the four distinct
source prefixes keep their outputs collision-free in the shared destination.

### 2. Naming: a group key, plus a sheet marker

The 16-hex digest already identifies an input file. Keeping it **shared across
one workbook's sheets** is what makes the outputs recognizably related, and
what makes §3 possible. `placement.rs` splits into a group key and two name
builders; the hash input is byte-for-byte unchanged:

```
group key    {sanitize_filename(file_stem)}-{sha256_hex(source_path \u{1} relative_path)[..16]}
single       {group}.csv
multi-sheet  {group}-{nn}of{tt}-{slug}.csv      nn, tt 1-based, zero-padded to 2
```

```
Budget.xlsx (3 non-empty sheets)            Solo.xlsx (1 sheet)
  Budget-3f9a2b7c1d4e5f60-01of03-Summary.csv    Solo-a1b2c3d4e5f60718.csv
  Budget-3f9a2b7c1d4e5f60-02of03-Q1-Detail.csv
  Budget-3f9a2b7c1d4e5f60-03of03-Notes.csv  notes.csv (copy-through)
                                                notes-9e8d7c6b5a493827.csv
```

A single-sheet workbook and a `.csv` copy-through keep **today's exact name**,
so the existing scheme is literally unchanged for every input with one output,
and the suffix appears only where it carries information.

- `nn` is the workbook's own sheet order, not an arbitrary enumeration -- it is
  the only place that ordering survives into the output.
- `slug` is the sheet name through `core::data::sanitize_filename`, then
  truncated to a new `MAX_SHEET_SLUG_LEN = 40`. A sheet name is untrusted text
  that may contain `/`, which `sanitize_filename` exists to neutralize
  (ADR-0013). Worst-case component is 100 (stem) + 1 + 16 + 1 + 8 + 1 + 40 + 4
  = **171 bytes**, inside the 255-byte per-component limit
  `MAX_FILENAME_LENGTH`'s own doc comment reasons about.
- ADR-0112 §5's unique-by-construction property is preserved rather than
  weakened: two workbooks differ in the digest, two sheets of one workbook
  differ in `nn`. Two sheets whose names sanitize to the same slug are still
  distinct names, so slug collisions are harmless by construction.
- **The destination stays flat.** `destination::list_existing_filenames` is
  non-recursive and keys on `entry.name`; a per-workbook subdirectory would
  have forced it recursive and re-keyed on `entry.path`. Encoding the grouping
  in the *name* rather than in a directory is what avoids that.

`deduplicate` solves its own one-input-many-outputs problem (zip expansion)
with `unique_path` plus a content index instead. That is unavailable here:
`unique_path` only sees the local `result/` tree, never the remote destination
(`placement.rs`'s module doc), which is exactly why `transform` derives names
deterministically and hard-errors on a collision instead of probing.

### 3. A group is done when its member count matches its declared total

The single-name `HashSet` membership test becomes a group index, built **once**
from the same destination listing, so the per-source-path cost stays O(1):

- `DestinationGroups::index(names)` -- for each existing name, locate the
  `-<16 hex>` digest token; everything through it is the group key. The
  remainder classifies as `.csv` (a plain single output) or `-NNofTT-....csv`
  (a member declaring a total). A name matching neither is a foreign object and
  is ignored rather than guessed at.
- `group_is_complete(group)` -- true if the plain name is present, or if
  members exist and `members.len() == declared_total` with every member
  agreeing on that total. Otherwise false.
- `files_needing_pull` and `enqueue` both call it with
  `group_key(source_path, relative_path)`, which needs no file content -- so
  ADR-0124's pre-download skip survives intact, which is the property worth
  protecting here.

**Why the total goes in the name.** Without it, "any member present" is the
only available test, and a workbook whose second sheet failed to push would be
treated as done forever, silently losing that sheet. Embedding `tt` makes a
half-pushed group *detectably* incomplete, so a rerun re-pulls and re-converts
it; the sheets that already landed are re-pushed and absorbed by `rclone
copyto`'s own skip-if-unchanged (ADR-0116). The run converges, and
`transform` still **never deletes at the destination** -- ADR-0112's stated
property, which the obvious alternative (delete the partial group, then redo)
would have broken for the first time.

`parse_sheet_marker` and `group_is_complete` are pure functions, unit-tested
independently of any listing, per ADR-0143's `circuit_breaker_should_trip`
precedent.

### 4. Conversion: a new `sheets.rs`, calamine-first

A new `src/commands/job/transform/sheets.rs`, leaving `media.rs` as the
ffmpeg-only module it is.

**`.xlsx`/`.xls`: `calamine`, in-process.** A pure-Rust reader for both, which
buys three things an external converter does not: the sheet list in workbook
order with exact names (§2's `nn` and `slug`), full control of the output
filenames, and tests that run in CI with no binary installed. Parsing is
CPU-bound and blocking, so it runs inside `tokio::task::spawn_blocking`
(ADR-0090).

**`.numbers`: `soffice --convert-to xlsx`, then the same calamine path.** No
Rust crate reads Apple's IWA format (a zip of Snappy-framed protobuf), and
LibreOffice's `libetonyek` import is the only portable reader. Routing it
through `.xlsx` rather than letting LibreOffice emit CSV directly is
deliberate: LibreOffice's all-sheets CSV export (filter token 12 = `-1`) names
its outputs by sheet *name* with no index, so workbook order would be
unrecoverable, and the filenames would be its choice rather than §2's. Using it
only as a format reader keeps **one** multi-sheet code path.

```text
soffice --headless -env:UserInstallation=file://<work_dir>/lo-profile \
  --convert-to xlsx --outdir <work_dir> <input>
```

- `-env:UserInstallation` is **not optional**: concurrent `soffice`
  invocations otherwise contend on one shared profile directory and either
  serialize or fail outright, and this job runs `--concurrency` files at once.
- The output file's **existence and non-emptiness** are verified rather than
  the exit code trusted; `soffice` exits 0 on some conversion failures.
- The call is wrapped in a timeout. Hanging is `soffice`'s known failure mode,
  and no existing converter in this job has one -- stated as a difference
  rather than a convention.
- Unlike ffmpeg there is no ADR-0115 muxer-inference trap, because §5 moves the
  scratch work into a *directory* and the input keeps its real `.numbers`
  extension, so nothing has to infer a format from a `.scratch` suffix.

**`.csv`: copy-through.** `media::copy_through` (a plain `fs::copy`), reused
unchanged, exactly as ADR-0112 handles a `jpeg` input already in the target
format. Encoding, line endings, delimiter, and quoting are left untouched: the
file is already valid CSV, and rewriting it would be a judgment call about
data that needs none. The run's value is the normalized, hash-named
destination tree.

**Cell rendering**, recorded here rather than left to be discovered from
output:

- Dates use calamine's `dates` feature and render ISO-8601. Without it an xlsx
  date is a bare serial (`45234`), which is the single most likely silent
  corruption in this whole path.
- Formula cells yield their **cached values** -- the stored `<v>`, which is
  what calamine reads and what we want. Formulas are not re-evaluated.
- Booleans render lowercase; a cell error renders empty with a `tracing::warn!`.

**Guards**, the analogue of the pixel-area and duration guards every other
converter has. A CSV has no intrinsic invariant like a duration, so the check
is structural: each emitted file's row count equals its sheet's used-range row
count, each file is non-empty, and the number of files equals `tt`.

- An **empty sheet is skipped and counted**, and `tt` is the count of
  *non-empty* sheets -- still a deterministic function of file content, which
  is all §3 requires. A workbook with zero non-empty sheets is a hard failure:
  a `0of0` group would make "complete" ambiguous.
- **Hidden sheets are included.** They hold data, and this job's business is
  moving data, not honoring a presentation flag.
- `MAX_SHEET_CELLS` caps a single sheet, because `worksheet_range`
  materializes it entirely in memory -- in the spirit of `archive.rs`'s
  `EXTRACTION_RATIO_LIMIT`, and the one place this path can be made to
  allocate without bound by its input.
- `~$*` Excel lock files are filtered at both dispatch choke points, the same
  shape ADR-0130 used for AppleDouble `._*` stubs: they match `*.xlsx`, are not
  workbooks, and would otherwise be failures rather than skips.

A new `NOT_A_WORKBOOK_SIGNATURE` joins both `NON_RETRYABLE_SIGNATURES` and
`is_wrong_input_kind_error`, so a misdirected run fails fast *and* drops its
local source copy per ADR-0143, and trips ADR-0121's circuit breaker rather
than grinding through a whole bucket.

### 5. `process_one` goes one-to-many

`src/commands/job/transform/worker.rs`:

- `ProcessResult` becomes `Result<Vec<PathBuf>, (FailureStage, String)>`.
  `retry_transcode_unless_fatal` is already generic over its success type, so
  the retry, non-retryable classification, and circuit breaker all work
  unchanged.
- Scratch becomes a per-input **directory**, `scratch/{group}/`, holding the
  LibreOffice profile, the intermediate `.xlsx`, and the per-sheet CSVs. The
  `{destination_name}.scratch` single-file naming disappears for this path.
  ADR-0127's clear-stale-scratch-at-startup still applies, now removing
  directories.
- ADR-0127's crash resume becomes per-group: if every expected result file is
  already in `result/`, skip conversion entirely; otherwise convert, and treat
  an individual already-present result as a legitimate skip (drop the scratch
  copy) rather than a `place_one` collision. `place_one`'s hard error stays
  exactly as it is for the genuinely different problem it exists for -- two
  distinct inputs hashing to one name.
- Push loops the outputs, acquiring **one semaphore permit per push** rather
  than holding one across a whole workbook. `--upload-transfers` keeps meaning
  "concurrent pushes", and no permit is held while a sibling converts.
  `push::push_one` is reused unchanged -- it was already per-output-file.
- ADR-0124's GC: each result is deleted immediately after its own successful
  push; the local source is deleted only once **every** sheet has pushed. Any
  failure retains the source for diagnosis, unchanged.
- `pushed` counts output objects (`+= outputs.len()`). The transcode-phase
  `record_phase_count(..., 1, ...)` stays `1`, because one *conversion* ran;
  the `pigeon_upload_*` metrics arrive per output object from `CopyTo::finish`
  for free. **No metric label value changes**, so dashboards are unaffected.
- `Outcome` needs no new variant: `Csv` -> `CopiedThrough`, the other three ->
  `Transcoded`, via `outcome_for`.
- `FileOutcome.destination_filename: Option<String>` becomes
  `destination_filenames: Vec<String>`, rendered comma-joined in
  `transform-report.txt` with the header renamed and an output count added to
  the summary line. A row stays **per input file**, so ADR-0130's pull
  reconciliation (which diffs the intended pull set against the dispatched set)
  needs no change at all.

### 6. The binary preflight moves after the input kind resolves

`dispatch_async` calls `media::check_ffmpeg_available()` *before*
`input_file_type` resolves, so every run demands `ffmpeg` whether or not it
will use it. A `csv`/`xlsx`/`xls` run needs neither `ffmpeg` nor `soffice`, and
a `numbers` run needs `soffice` and not `ffmpeg`. The preflight moves to
immediately after the input kind resolves and dispatches on it: `ffmpeg` for
the image, video, and audio kinds, `soffice` for `Numbers`, nothing extra for
`Xlsx`/`Xls`/`Csv`.

This keeps the property the original ordering was protecting -- fail on a
missing binary before any prompt, not partway through a long pipeline -- while
dropping a requirement a given run does not have. `input_file_type` already
resolves and hard-errors strictly before every other input, so there is no new
ordering risk.

### 7. The four hardcoded `--input-file-type` lists

There is no clap `ValueEnum`; the flag is `Option<String>` parsed by hand. A
new variant must be added in **four** places, listed so none is missed:
`format.rs`'s `parse` match, `parse`'s error text, `wizard.rs`'s interactive
`options` array, and `cli.rs`'s flag doc block.

`format.rs`'s variant-iterating tests (`every_other_kind_has_exactly_one_spelling`,
`every_kinds_display_name_parses_back_to_itself`,
`output_extension_is_jpg_for_image_kinds_and_mp4_for_video_kinds`) are
hand-written lists, not exhaustive iteration, so they would silently *not*
cover a new kind. Each is extended.

### 8. Dependencies and deployment

- `Cargo.toml` gains `calamine` (with `dates`) and `csv`. This re-adds
  `quick-xml` transitively, which ADR-0131 pruned when `pull-transform` went --
  stated rather than left to be noticed in `Cargo.lock`. A pure-Rust reader is
  also friendlier to the crates.io publish (ADR-0018) than a fourth external
  binary would be.
- The `Dockerfile` runtime stage gains `libreoffice-calc`: hundreds of MB on a
  `debian:bookworm-slim` arm64 base (ADR-0087), for one input kind. That cost
  is the price of `.numbers` at all, and is the reason §4 confines LibreOffice
  to that kind rather than using it for all four.
- **Found while checking that layer, and not fixed here**: the image installs
  `ffmpeg` but **not `rclone`**, which every job requires on `PATH`
  (`prepare_rclone`, ADR-0145 §5). Either the image cannot run any job or
  production provisions `rclone` by some path this repo does not record. Filed
  per ADR-0031 rather than fixed in a spreadsheet ADR ([#96](https://github.com/noisypigeon/pigeon-cli/issues/96)).

## Consequences

- `transform` gains a fourth output extension and, more significantly, its
  **first input kind where one input yields several outputs**. Four of the five
  places that assumed otherwise change shape; the fifth
  (`list_existing_filenames`) survives only because §2 keeps the destination
  flat.
- `destination:csv/` holds two naming shapes -- plain for a single output,
  `-NNofTT-slug` for a multi-sheet workbook. Deliberate (§2), so the suffix
  carries information only where there is information to carry, but an operator
  reading that directory sees both.
- **A pre-existing run's names are unaffected.** Image, video, and audio kinds
  go through the unchanged single-output path and produce byte-identical names,
  so no already-written bucket needs remediation and no rerun redoes work.
- `Outcome::Transcoded` now covers "split a workbook into CSVs", which reads
  oddly. Kept anyway: `"transcoded"` is a metric label *value*, and changing it
  would break continuity in the existing `pigeon_job_phase_total` series for
  no behavioral gain (ADR-0148's standard).
- A `transform` run can now require `soffice` instead of `ffmpeg`, the first
  time this job's external-tool requirement depends on its own flags. §6 makes
  that requirement exact rather than a union.
- `pigeon` depends on LibreOffice's `libetonyek` for `.numbers`. A build
  without it fails per-file with a message naming the input kind, which
  `NON_RETRYABLE_SIGNATURES` classifies and the circuit breaker turns into a
  run abort naming an environment-capability gap -- the ADR-0142 mechanism,
  requiring no new code.
- **Recorded as not verified**: whether `libetonyek` reads *recent* `.numbers`
  files faithfully. Its Numbers support is known to be incomplete, `soffice` is
  not installed on the machine this ADR was written on, and no real `.numbers`
  file was converted. This is why the Status is **Proposed**: the Verification
  section gates it on a real file. If it fails, `Numbers` is dropped to an
  out-of-scope issue and the other three kinds ship unchanged -- §4 confines
  LibreOffice to that one kind precisely so that subtraction is clean.
- A `.numbers` saved as a **package directory** rather than a single file
  uploads as many S3 objects under `Foo.numbers/` and will not match
  `--include '*.numbers'`. Only single-file packages are in scope; such a tree
  is silently invisible to the source listing, which is the ADR-0139 failure
  shape -- mitigated only by the zero-match warning that ADR already added.

## Out of scope

Filed per ADR-0031 where genuinely deferred rather than a permanent boundary:

- **`.numbers` package directories** (see Consequences). Deferred, not
  rejected -- it needs source-listing changes, not a converter change ([#94](https://github.com/noisypigeon/pigeon-cli/issues/94)).
- **`xlsm`, `xlsb`, `ods` aliases.** `calamine` reads all three, but ADR-0139's
  rule is that an alias is added on evidence of a real miss. Cheap to add when
  one appears ([#95](https://github.com/noisypigeon/pigeon-cli/issues/95)).
- **CSV normalization** -- transcoding encoding, delimiter, or line endings on a
  `.csv` input. Settled as copy-through in §4: the input is already valid, and
  delimiter/encoding sniffing is a new source of silent corruption.
- **A per-run output-format menu** (TSV, JSON, Parquet) mirroring
  ADR-0122's `--video-quality`. One fixed output format, per ADR-0112's
  deferral of `--output-file-type` until a second one actually exists.
- **Merging a workbook's sheets into one CSV.** The sheets are separate tables;
  concatenating them would invent a schema.
- **Formula re-evaluation.** Cached values only (§4).
- **Cell formatting, styles, merged-cell geometry, charts, and embedded
  images.** CSV cannot express any of it; this is a data extraction, and saying
  so is more honest than a partial attempt.
- **Making `media::check_ffmpeg_available` check the exit status**, not
  merely that the process spawned, so a present-but-broken `ffmpeg` fails
  the preflight rather than every file later. §4's new
  `check_soffice_available` deliberately does not copy that shape, but
  fixing the existing one is orthogonal to spreadsheets and would widen this
  change into the path every media run depends on ([#100](https://github.com/noisypigeon/pigeon-cli/issues/100)).
- Pulling both members of a case-colliding key pair -- already
  [#83](https://github.com/noisypigeon/pigeon-cli/issues/83), unchanged by this
  ADR.

## Amends

ADR-0112 (its `compute_destination_name` splits into a group key plus two name
builders, and its one-output-per-input premise no longer holds for every kind;
its flat destination, no-deduplication, never-delete, and
unique-by-construction properties all survive), ADR-0120 and ADR-0124 (the
destination listing stays the sole "already done" authority, but the test
becomes a group-completeness question rather than single-name membership),
ADR-0122 (a third non-media input kind in the enum it extended to video), and
ADR-0139 (its extension-set model gains four single-spelling kinds). Each gets
a short forward-pointing "Amendment" section, per this repo's convention for
amending a merged ADR without rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `format.rs`: each new kind parses case-insensitively, rejects neighbouring
  spellings (`xlsx2`, `number`, `excel`), has exactly one extension,
  `output_extension()` is `csv`, `is_video()` is false, `is_spreadsheet()` is
  true, `name()` round-trips through `parse`, and `Xls` does not match `xlsx`.
- `placement.rs`: the group key is stable across repeated calls; every sheet of
  one input shares it; two inputs sharing a basename in different directories
  do not; a single-sheet name is byte-identical to today's; a worst-case stem
  and sheet name stay under 255 bytes.
- `DestinationGroups`: complete, incomplete, plain-name, disagreeing-total, and
  foreign-name cases; `parse_sheet_marker` against a stem that itself contains
  `-` and against a 16-hex-looking stem fragment.
- `sheets.rs`: a fixture `.xlsx` with 3 sheets (one empty) yields exactly 2
  CSVs named `01of02`/`02of02` in workbook order; a single-sheet workbook
  yields the plain name; a date cell renders ISO-8601; a formula cell renders
  its cached value; a zero-non-empty-sheet workbook, a truncated file, and a
  password-protected file each fail with a message
  `is_non_retryable_transcode_error` classifies; `MAX_SHEET_CELLS` trips; a
  `~$lock.xlsx` is skipped, not failed.
- `worker.rs` end-to-end: a multi-sheet source pushes every sheet and reports
  them all on one row; a rerun is a clean no-op; **a destination whose group is
  artificially left incomplete re-pulls and completes it** -- the §3 regression
  test, and the reason the total is in the name; local source and results are
  gone after success, and the source is retained after a failure.
- `tests/cli.rs`: `transform --help` lists `numbers`, `xlsx`, `xls`, `csv`; an
  invalid value still fails fast non-interactively; a `csv` run does not
  require `ffmpeg` on `PATH` (§6).
- Manual, per kind: a real run against the live buckets confirming
  `destination:csv/` contents, sheet ordering against the source workbook, and
  `transform-report.txt` rows.
- **Gating this ADR's Status**: a real `.numbers` file, saved by a current
  Numbers version, converted end-to-end with its sheet names and cell values
  checked by hand against the source. `Proposed` -> `Accepted` only on that
  evidence; on failure, `Numbers` is removed and refiled.

## Amendment (2026-10-14): four corrections found while implementing this

Implementing §1-§8 turned up four places where this ADR as merged needed
correcting. The first is a genuine bug in it; the rest preserve its intent
with fewer moving parts. Recorded here rather than diverged from silently.

**1. The sheet slug needs its own sanitizer; `sanitize_filename` is not
enough.** §2 specifies the slug as the sheet name "through
`core::data::sanitize_filename`". That function does **no character
filtering whatever** — it takes the final path component and truncates to
100 bytes, nothing else. So a sheet named `Q1,Q2` keeps its comma, and a
sheet name containing a **tab or newline** would be written straight into
`transform-report.txt`, whose format is tab-separated and
one-line-per-row, corrupting it. Sheet names are author-controlled text, so
this is the bug class ADR-0033 fixed for `yaml_quote`, and it is fixed the
same way: restrict the character set at the producer rather than escape at
each consumer. A new `core::data::sheet_slug` keeps `[A-Za-z0-9._-]`, maps
everything else to `-`, collapses runs, trims, truncates to 40 bytes, and
falls back to `"sheet"`. §5's comma-joined report column is unambiguous
only *because* that set excludes `,`; the dependency is noted at the join.

**2. No `dates` feature, and therefore no `chrono`.** §4 specifies
ISO-8601 dates "via calamine's `dates` feature". That works but is
unnecessary and would add a second date library beside the `time` crate
already here. `calamine`'s `formats.rs` constructs
`Data::DateTime(ExcelDateTime::new(value, type, is_1904))` with **no
`cfg(feature)` anywhere in the file**, and `ExcelDateTime::to_ymd_hms_milli`
is **ungated** and applies the 1904 epoch itself; only
`as_datetime()`/`as_date()` are `#[cfg(feature = "chrono")]`. So ISO-8601
costs zero added dependencies.

Worth stating why the *other* obvious shortcut is wrong: `is_1904` is a
private field with no public accessor, so converting from `as_f64()` by
hand would silently shift a Mac-authored workbook by 1462 days — four years
and a day. A test pins both epochs apart. Confirmed in the same reading
that `impl Display for ExcelDateTime` writes the bare serial, so §4's named
trap (`45234` where a date belongs) is real rather than theoretical, and a
test pins that too.

**3. `MAX_SHEET_CELLS` bounds the output, not the peak allocation.** §4
frames the cell cap as the memory guard. It cannot be:
`worksheet_range` materialises an entire sheet before `get_size()` can be
asked, so by the time the cap could fire the allocation has happened. The
cap is kept — it still turns a pathological sheet into a clear,
non-retryable error — but the real bound is a new pre-parse check on the
input file's size (`MAX_WORKBOOK_BYTES`), which is cheap and genuinely
pre-emptive.

**4. Duration cells were unaddressed.** `ExcelDateTimeType` has a
`TimeDelta` variant, and `to_ymd_hms_milli` on one is meaningless — it is
elapsed time, not a date. Rendered as `HH:MM:SS` from the fractional-day
value, the way Excel displays it, with hours deliberately not wrapped at 24.

### Smaller corrections

- **§7 undercounts the hardcoded kind lists at four; there are six**, plus
  three more in tests. Beyond `format.rs`'s `parse` match, its error text,
  `wizard.rs`'s `options` array, and `cli.rs`'s `--input-file-type` doc, the
  kind→output-extension mapping is *also* spelled out in `cli.rs`'s
  `--destination` doc and in the `Transform` variant's own doc block. In
  tests: `worker.rs`'s `outcome_for_…` and `media.rs`'s two signature-list
  tests. All nine updated.
- **The circuit-breaker abort message hardcoded `ffmpeg`.** Consequences is
  right that a missing `libetonyek` needs no new code to become a run
  abort, but the message said "the ffmpeg build on this job's host", which
  would misname the problem for a `.numbers` run. Now names the converter
  generically.
- **§6's "fail before any prompt" holds only because the whole
  `InputFileTypeInput` block moves above `load_keyring`.** `prompt()` is
  itself a `Select`, so checking the binary merely "after the input kind
  resolves" would put it after a prompt. `InputFileTypeInput` reads no
  keyring state, so hoisting it is safe and keeps the property intact.
- **`NOT_A_WORKBOOK_SIGNATURE` lives in `media.rs`**, not `sheets.rs`,
  bending §4's "keeps `media.rs` ffmpeg-only" slightly.
  `NON_RETRYABLE_SIGNATURES` is a *function-local* const, so it can only be
  appended to there; and that module's const doc comment exists precisely to
  stop the producer and both matchers drifting apart.
- **§5's per-group crash resume needed a decision §5 left open.** "Every
  expected result already in `result/`" is unanswerable without the sheet
  count, which means opening the workbook — defeating the point. Resolved by
  applying `DestinationGroups::index` to a listing of the *local* `result/`
  tree, exactly as §3 applies it to the remote one: the `-NNofTT-` markers
  already present declare their own total.
- **The per-input scratch directory gets an RAII `Drop` guard**, which §5
  does not mention. `process_one` has many `?` early-returns, and a
  `.numbers` conversion's scratch directory holds a whole LibreOffice user
  profile. Without the guard the only cleanup is the startup clear, so a
  long run would accumulate one profile per converted workbook until the
  *next* run — real disk exhaustion on the ephemeral VM this job targets.
- **`place_one` stays on the single-output path.** Routing everything
  through the resume-tolerant `place_sheet` would have silently dropped
  ADR-0112 §5's deliberate hard-error-on-collision for media. The tolerance
  is confined to the kind that needs it.
- **The circuit breaker samples per input file, not per output**, so a
  workbook yielding 12 CSVs is one sample. Correct — it detects a systemic
  converter gap — but its 20-sample window now covers fewer output objects
  for a spreadsheet run than for a media one.

## Amendment (2026-10-14): two of this ADR's own fixes were wrong (ADR-0151)

The first real `--input-file-type xlsx` run (266 files → 1318 CSVs, 0
transcode/place/push failures) worked, and this ADR's naming, resume, and
RAII scratch guard all behaved as designed. Reading its artifacts found two
defects in this ADR itself.

**The comma-joined report column is ambiguous.** The amendment above claims
the join is unambiguous "only because that set excludes `,`", referring to
`sheet_slug`. That reasoning covers the *slug* half of a destination name
and misses the *stem*, which comes from `sanitize_filename` -- a function
that does **no character filtering whatever**. The run produced
`Budget (Normal, Reduced, Crisis)-ee7a9c8a4c2e5342.csv`: one filename
containing two commas, in a comma-separated column, reading as three. 5 of
267 source names in that run contain a comma.

ADR-0151 adds `core::data::strict_stem` and applies it in `group_key` **for
spreadsheet kinds only** -- sufficient because a separator can only mislead
where the column holds more than one value, and necessary because
`group_key` feeds every kind's destination names while this job never
deletes, so widening it would duplicate ~25,000 objects in the jpg bucket
rather than rename them.

**The lock-file skip was counted two different wrong ways.**
`files_needing_pull` added a `~$` stub to `skipped_apple_double`, so the log
and report called it an AppleDouble sidecar -- that run reported "2
AppleDouble sidecar(s)" with no way to tell which kind they were. And
`enqueue` returned `SkippedLockFile` while all three call sites matched only
on `SkippedAppleDouble`, so a stub reaching that choke point was counted
nowhere at all. ADR-0151 gives it its own counter on both paths.

Also from that run, and not this ADR's fault: the per-output push logs this
ADR multiplied ~5× were never cleaned (1318 left behind), which ADR-0151
fixes under ADR-0124's GC.
