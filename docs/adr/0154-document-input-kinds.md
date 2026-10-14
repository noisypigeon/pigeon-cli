# ADR-0154: document input kinds, and `transform`'s first input family with three output extensions

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-14.
- **Status**: Proposed.

## Context

`job run transform` normalizes one input family per run to a single canonical
output: images to `.jpg` (ADR-0112), video to `.mp4` (ADR-0122), audio to
`.opus` (ADR-0142, ADR-0153), spreadsheets to `.csv` (ADR-0149). Documents are
the one large category of the archive with no path at all.

This ADR adds thirteen variants across fifteen CLI spellings, and with them the
first input family whose members do **not** share one output extension:

| Output | Kinds | Route |
|---|---|---|
| `.pdf` | `doc`, `docx`, `pages`, `rtf`, `pptx`, `key` | `soffice --convert-to pdf` |
| `.pdf` | `epub` | `ebook-convert` (Calibre) |
| `.pdf` | `pdf` | copy-through, `media::copy_through` |
| `.txt` | `md`/`markdown`/`mkd`, `rst` | `pandoc -t plain` |
| `.txt` | `textclipping` | in-process, binary property list |
| `.txt` | `txt` | copy-through |
| `.csv` | `tsv` | in-process, the existing `csv` crate |

Three things found while grounding this shaped it more than the variant list
did, and all three were measured rather than assumed.

### A `.key` holding a private key converts cleanly, and the key is legible in the PDF

`.key` is Apple Keynote, and also the conventional extension for a PEM private
key. The question was whether LibreOffice would reject the second. It does not.
Given a file containing an OpenSSH private key named `secret.key`:

```text
convert /tmp/secret.key as a Writer document -> /tmp/out/secret.pdf
        using filter : writer_pdf_Export
exit 0, 17558 bytes
```

It imports the PEM as a plain-text Writer document, **exits 0**, and writes a
valid one-page PDF. Rendering that PDF back to an image shows the key verbatim
-- the `-----BEGIN OPENSSH PRIVATE KEY-----` banner and every base64 line of
key material, legible.

Every check this codebase would have applied passes. ADR-0149's
`convert_numbers_to_xlsx` treats "a non-empty file where the next stage expects
one" as success precisely *because* the exit code is unreliable, and here there
is a large, well-formed, non-empty PDF. So without §2 a `--input-file-type key`
run would transcribe private keys into PDFs and push them to the destination
bucket, where nothing distinguishes them from legitimate Keynote output. The
guard is a prerequisite, not a refinement.

### EPUB is the one kind LibreOffice cannot read

LibreOffice 26.8.1 carries `libetonyek-0.1.1.dylib`, so `.pages` and `.key`
import for the same reason `.numbers` does (ADR-0149 §4). Its filter registry
confirms an IMPORT filter for every kind routed to `soffice` above: `MS Word
97`, `MS Word 2007 XML` / `Office Open XML Text`, `Rich Text Format`, `Impress
MS PowerPoint 2007 XML`, `Apple Pages`, `impress_AppleKeynote`.

EPUB is the exception, and not by a near miss:

```text
node  writer_EPUB_Document   ext=epub
node  EPUB   flags=EXPORT ALIEN 3RDPARTYFILTER   type=writer_EPUB_Document
```

`EXPORT` with no IMPORT counterpart anywhere in the registry. LibreOffice
*writes* EPUB and cannot read it. That single fact is why `epub` takes a
different converter from the six kinds it otherwise sits beside, and why this
ADR adds two external binaries rather than one.

### A modern `.textClipping` is a binary property list, not a resource fork

A `.textclipping` is the file macOS creates when text is dragged to the Finder,
and the expectation going in was a resource fork holding a flattened pasteboard
-- which would have made it awkward to read anywhere but macOS. Measured
against a real sample, that is not how it is stored. The file is 295 bytes, has
**no** resource fork (`..namedfork/rsrc` does not exist), and `file` reports
`Apple binary property list`. Its structure:

```text
UTI-Data
  com.apple.traditional-mac-plain-text  -> bytes
  public.utf8-plain-text                -> string
  public.utf16-plain-text               -> bytes
```

One top-level key, three encodings of the same text, one of them already UTF-8.
So extraction is a property-list read, with no platform dependency and no
external binary -- which is what makes it a cheap kind to include rather than
the awkward one it looked like.

## Decision

### 1. Thirteen variants, fifteen spellings

`Doc`, `Docx`, `Pages`, `Rtf`, `Pdf`, `Epub`, `Pptx`, `Key`, `Txt`, `Md`,
`Rst`, `Textclipping`, `Tsv`.

`doc` and `docx` are **disjoint variants**, not two spellings of one kind: a
`.doc` is binary CFBF and a `.docx` is OOXML, different formats reached by
different LibreOffice filters. This follows `xls`/`xlsx`, which `format.rs`
already pins with `xls_and_xlsx_are_disjoint_kinds`.

`md` is the opposite case and the first kind with **three** spellings --
`md`, `markdown`, `mkd` are one format under different extensions, so they are
one variant, generalizing `Jpeg`'s existing `["jpg", "jpeg"]`. Multi-spelling
costs nothing downstream: `extensions()` already feeds `Lsjson`'s
`include_extensions`, which rclone matches with `--ignore-case` -- which is
also why `.textClipping`'s capital C needs no special handling.

No other aliases. `odt`, `odp`, `ods`, `pps`, `text` are left out per
ADR-0139's add-on-evidence rule.

Prefix inference needs no new code, and gives the fan-out for free:
`--input-file-type docx` resolves `source:docx/` to `destination:pdf/`, while
`--input-file-type tsv` resolves `source:tsv/` to `destination:csv/`.

### 2. A `.key` that is not a Keynote file is refused before `soffice` sees it

Given the Context measurement, the guard cannot be a check on the converter's
output -- the output is a perfectly good PDF. It has to reject the input.

Before conversion, a `Key` input is sniffed: a modern Keynote is a zip
(`PK\x03\x04`), and anything else is refused with `NOT_A_KEYNOTE_SIGNATURE`.
The check is *positive* -- it requires the iWork container magic rather than
blocklisting `-----BEGIN` -- because the set of things that are not Keynote is
open-ended and a blocklist would have to anticipate PKCS#8, PKCS#12, DER and
whatever else arrives with a `.key` extension.

This is the ADR-0141/0142 `has_video_stream` guard shape, applied to a case
where the cost of a false negative is a leaked secret rather than a wasted
transcode, and it composes with ADR-0143 exactly as intended: the failure is
classified wrong-input-kind, so the local source copy is deleted and the key
does not linger on the transform host. The source object is untouched at
`--source`, which is where it was already.

**The guard does not make the kind safe to point at a key directory.** It
prevents a key being *converted*, not a key being *pulled*. An operator who
runs `--input-file-type key` over a prefix holding private keys still pulls
them to local disk before each is refused. Said plainly here because the
feature's name invites exactly that mistake.

### 3. Destination naming is unchanged, and that is the collision answer

`placement::compute_destination_name` is `{stem}-{sha256[..16]}.{output_ext}`
over `(source_path, source_key)`. Collisions are impossible by construction
(ADR-0112 §5), so a `pdf` copy-through needs no new disambiguation, including
across the seven kinds that now converge on one `destination:pdf/` prefix --
`doc/report.doc` and `docx/report.docx` hash differently because their source
keys differ.

**`strict_stem` is deliberately not widened to the document kinds.** ADR-0151
scoped it to `is_spreadsheet()` because only a multi-output kind has a
comma-joined report column that a comma in a filename can misread, and every
document kind here is single-output. Widening it would be actively harmful
rather than merely unnecessary: this job never deletes at the destination, so
renaming would **duplicate** rather than migrate, which ADR-0151 measured at
67% of inputs. A test pins that a document destination name is byte-identical
to today's media naming.

### 4. No `is_document()`, and `is_spreadsheet()` is not reused

`is_spreadsheet()` is consulted in three unrelated places -- `strict_stem`
selection in `group_key`, `place_sheet` versus `place_one`, and converter
dispatch. A `tsv` is spreadsheet-ish and would be a plausible member, but it is
single-output, so classifying it that way would be wrong in two of those three.
It is a document kind that happens to output `.csv`, dispatched on its own arm.

Nor is there an `is_document()` to match `is_video()`/`is_audio()`/
`is_spreadsheet()`. Those predicates work because each family has one output
extension, so the predicate and the branch coincide. Documents span three, so a
single boolean cannot select the branch and would only invite the call sites to
re-derive the real distinction. `output_extension()` already carries it.

This is the limit of ADR-0153's "ask what a kind *is*" shape, recorded here so
the next family does not reach for a predicate that cannot carry its weight.

### 5. The converter preflight becomes exhaustive, closing an undocumented trap

`required_converter_available` ends in `_ => check_ffmpeg_available()`. Any
variant added without touching it silently demands `ffmpeg` and never checks
its real converter -- so a `docx` run would pass preflight on a host with no
LibreOffice and fail on the first file. ADR-0149 §7 listed four touchpoints
with no compiler help and did not know about this fifth; it was added by that
same ADR.

It is replaced with an exhaustive `match` returning a `Converter`
(`None`, `Ffmpeg`, `Soffice`, `Calibre`, `Pandoc`), making it the compiler's
problem instead of the next author's. `pdf`, `txt`, `tsv`, `textclipping`,
`csv`, `xlsx` and `xls` require nothing.

The remaining four touchpoints are unchanged and still need care: `parse`'s
string match, `parse`'s error text, `wizard.rs`'s `options` array, and
`cli.rs`'s flag doc block.

### 6. A new `soffice.rs`, parameterized from `sheets.rs`'s runner

`sheets::convert_numbers_to_xlsx` already solves every problem the document
path has, and the reasoning is in the code: a per-invocation
`-env:UserInstallation` profile, *"not optional ... this job runs
`--concurrency` files at once"*; a 300-second timeout, because *"LibreOffice's
documented failure mode is hanging rather than exiting non-zero"*; and output
verification that does not trust the exit code, because *"soffice exits 0 on
some conversion failures"*. The Context measurement is that last comment being
right in a new way.

It moves to `transform/soffice.rs` as `convert(input, work_dir, target_ext,
signature)` plus `check_available()`, with `sheets.rs` calling it for `xlsx`
and `documents.rs` for `pdf`.

Two consumers, not the three this repo's hoisting precedent asks for. The
distinction is that this is a **parameterization of one existing function**
rather than a speculative generalization -- the alternative is a byte-for-byte
copy differing in one string literal, which is what the precedent exists to
prevent. Stated rather than left to look like an oversight.

The ADR-0115 scratch-suffix trap does not apply, for the reason `sheets.rs`
already records: `soffice` derives its own output name inside `--outdir`, so
unlike every `ffmpeg` call -- which needs an explicit `-f` because `worker.rs`
hands it a path ending in `.scratch` -- the soffice route must *not* use a
`.scratch` suffix.

### 7. A new `documents.rs` for the four remaining routes

- **`epub` to PDF via `ebook-convert`.** Calibre is purpose-built for ebook
  conversion and is the highest-fidelity option available. `pandoc` was
  considered and rejected: `pandoc -o out.pdf` requires a LaTeX engine, which
  is a heavier dependency than Calibre and a worse result for reflowable text.
- **`md` and `rst` to text via `pandoc -t plain`.** One tool covers both, which
  is the deciding factor -- `pulldown-cmark` would handle Markdown in-process
  but reStructuredText has no mature Rust parser, so the alternative is two
  mechanisms for one output family.
- **`textclipping` in-process**, reading `UTI-Data` then
  `public.utf8-plain-text`, falling back to
  `com.apple.traditional-mac-plain-text` when absent. A clipping that is not a
  property list -- a pre-OS-X resource-fork one -- is refused with
  `NOT_A_TEXTCLIPPING_SIGNATURE` rather than guessed at.
- **`tsv` to CSV in-process**, `csv::ReaderBuilder::delimiter(b'\t')` into
  `csv::Writer`, reusing the dependency ADR-0149 added with the note that
  *"Quoting/escaping untrusted cell text is not worth hand-rolling"*. That is
  also why this is a real conversion and not a byte substitution: a field
  containing a comma has to become a quoted CSV field, and a naive tab-to-comma
  replacement would silently corrupt the table.

Both new binary checks follow `check_soffice_available`'s form, **checking the
exit status** and not merely that the process spawned -- the hole
`check_ffmpeg_available` still has and whose own doc comment admits it
([#100](https://github.com/noisypigeon/pigeon-cli/issues/100)).

### 8. Markdown and reStructuredText lose their markup, deliberately

`pandoc -t plain` renders to prose: `# Heading` becomes `Heading`, `**bold**`
becomes `bold`, and `[text](url)` becomes `text` with **the URL discarded**.
This is lossy and irreversible, and it was chosen over the alternative of
copying the bytes unchanged under a `.txt` extension.

The reason to record it: Markdown and reStructuredText *are* plain text, so
copy-through would have been lossless and free. The choice is that this family
is defined by its output being readable prose, consistent with `pdf` being the
output for the document family rather than a pile of source formats. An
operator who wants the markup preserved should not run this job on those files
at all -- there is no flag, and `--output-file-type` remains deferred per
ADR-0112.

`txt` itself is copy-through, since there is no markup to strip.

### 9. `Outcome` needs no new variant

`pdf` to `pdf` and `txt` to `txt` are `CopiedThrough`, joining `Jpeg` and
`Csv`. Everything else is `Transcoded`, including `tsv` to `csv` and `md` to
`txt` -- both genuinely rewrite their content, so neither is the `Opus` case
ADR-0153 had to argue about.

### 10. Dependencies and deployment

One new direct crate, `plist`, for §7's clipping read. `quick-xml` is **not**
re-added -- it is present only transitively via `calamine`, and nothing here
parses XML directly.

The Dockerfile is the larger cost. It installs `libreoffice-calc`, which does
**not** open Writer or Impress formats, so six of these kinds would fail on
today's image. It gains `libreoffice-writer`, `libreoffice-impress`, `calibre`
and `pandoc`. ADR-0149 already called LibreOffice *"the heaviest thing in this
image by far"*; this change makes the image substantially heavier again, and
that is accepted rather than worked around, because the alternative is a job
that cannot run where it is deployed.

[#96](https://github.com/noisypigeon/pigeon-cli/issues/96) -- the Dockerfile
installs `ffmpeg` but not `rclone`, which every job requires -- is still open
and is in the layer this change touches. It is not fixed here, only noted
again.

## Consequences

- Thirteen new input kinds, taking `InputFileType` from 16 variants to 29 and
  `transform` from four input families to five.
- **A private key named `.key` is refused rather than published** (§2). This is
  the change that matters most and the one with no prior equivalent: every
  other guard in this job prevents a bad conversion, this one prevents a
  disclosure.
- Two new external binaries and two new LibreOffice packages, against zero for
  ADR-0153 and one for ADR-0149. This is the largest dependency addition the
  job has taken.
- **Per-file cost rises sharply for the six `soffice` kinds.** Measured: a cold
  per-invocation profile costs 1.07 s against 0.45 s warm, so roughly 0.6 s of
  every file goes to building a LibreOffice profile that ADR-0149's `ScratchDir`
  then deletes -- 48 files and 492 KB per input. That is the price of the
  concurrency fix, not a regression, but it means a document run is far more
  expensive per file than an image run and `--concurrency` should be set with
  that in mind.
- The seven kinds reaching `.pdf` all write to one `destination:pdf/` prefix.
  Intended, and safe because §3's naming is collision-free, but it means the
  destination no longer tells you which kind produced an object.
- `tsv` previously appeared in a test asserting it does **not** parse; that
  assertion is reversed, with a comment saying so (the same reversal ADR-0153
  made for `opus`).
- `md`/`rst` output loses link URLs and all inline markup (§8).
- `pandoc` and `ebook-convert` are absent from the authoring machine, so two of
  the three routes are unverified. See Verification.

## Out of scope

Filed per ADR-0031 where genuinely deferred rather than a permanent boundary:

- **Pre-OS-X resource-fork `.textclipping` files.** The sample measured here is
  a data-fork property list; the older form stores a flattened pasteboard in a
  resource fork and needs a different reader. Refused clearly rather than
  guessed at, and deferred until one is known to exist in the archive. ([#110](https://github.com/noisypigeon/pigeon-cli/issues/110))
- **OCR of scanned or image-only PDFs.** A `pdf` input is copied through, so a
  scanned document stays an image of text. Extracting it is a different kind of
  work from format conversion and would need its own converter and its own
  accuracy discussion. ([#111](https://github.com/noisypigeon/pigeon-cli/issues/111))
- **A `.pages` or `.key` saved as a package directory.** Older iWork formats are
  directories, which the source listing cannot see as one file -- identical in
  cause to [#94](https://github.com/noisypigeon/pigeon-cli/issues/94) for
  `.numbers`, which this ADR does not change.
- **OpenDocument `odt`/`odp` input kinds.** Already covered by the same
  add-on-evidence reasoning as
  [#95](https://github.com/noisypigeon/pigeon-cli/issues/95)'s `ods`; added when
  a real run is observed to miss files.
- **A present-but-broken `ffmpeg` passing preflight**
  ([#100](https://github.com/noisypigeon/pigeon-cli/issues/100)) -- unchanged by
  this ADR. The three soffice/Calibre/pandoc checks do test exit status, so the
  gap is now `ffmpeg`-only.
- Preserving Markdown and reStructuredText markup (§8, decided with reasoning).
- Password-protected `.docx`, `.pages` or `.key` -- these fail at the converter,
  and prompting for a password has no place in a batch job.
- PDF/A normalization, or any re-encoding of a `pdf` input. Copy-through is the
  decision; a PDF is already the target format.
- An `--output-file-type` menu. ADR-0112 deferred it until a second output
  format per family is actually wanted, and that is still not the case.

## Amends

ADR-0112 (two more copy-through kinds, `pdf` and `txt`, joining `jpeg` and
`csv`), ADR-0149 (its `soffice` runner is parameterized and moves to its own
module, and its four-touchpoint list gains a fifth that is now
compiler-enforced), ADR-0151 (`strict_stem`'s spreadsheet-only scope is
deliberately preserved, with the reasoning restated), and ADR-0153 (its
one-predicate-per-family shape is recorded as not generalizing to a family with
three output extensions). Each gets a short forward-pointing "Amendment".

## Verification

- `mise run ci` clean (fmt-check + lint + test), with the new test count
  recorded.
- `format.rs`: all thirteen kinds parse case-insensitively; `md`, `markdown`
  and `mkd` resolve to one kind whose `canonical_extension()` is `md`; `doc`
  and `docx` are disjoint; each of the three output families asserts its
  `output_extension()`; `odt`, `odp`, `ods`, `pps`, `text`, `markdown2` still
  reject; and the `tsv` rejection assertion is reversed with a comment.
- `placement.rs`: **a document destination name is byte-identical to today's
  media naming** -- the assertion that pins §3's decision not to widen
  `strict_stem`.
- `soffice.rs`: the parameterized runner still produces `.xlsx` for Numbers, so
  ADR-0149's spreadsheet tests pass unchanged, and `convert` fails clearly when
  no usable output appears. Gated on `check_soffice_available`, never on
  `check_ffmpeg_available` -- gating a non-ffmpeg kind on ffmpeg is the
  ADR-0139 mistake ADR-0149 named.
- `documents.rs`: `tsv` to `csv` is pure and fully testable with no binary,
  **including a field containing a comma becoming a quoted CSV field**;
  `textclipping` parses a fixture built in-test via the `plist` crate, never a
  committed binary; **a `.key` whose bytes are `-----BEGIN OPENSSH PRIVATE
  KEY-----` is refused**, and its error satisfies both
  `is_non_retryable_transcode_error` and `is_wrong_input_kind_error` -- the §2
  regression test, and the one this ADR exists to guarantee.
- `media.rs`: all three new signatures are non-retryable and wrong-input-kind,
  with the messages built from the consts so producer and matcher cannot drift.
- `worker.rs` end-to-end: a `tsv` run and a `textclipping` run each transcode,
  push, and rerun as a no-op (`rerun.pushed == 0`,
  `skipped_already_done == 1`), leaving `source/`, `result/`, `scratch/` and
  `push-logs/` empty -- the ADR-0124/0149/0151 cluster. A `pdf` run reports
  `CopiedThrough` with byte-identical output.
- `tests/cli.rs`: `transform --help` lists every new spelling, backticked so an
  incidental substring cannot satisfy the assertion; an invalid kind fails fast
  and the error lists the valid ones; a `tsv` run requires neither `ffmpeg` nor
  `soffice`; a `docx` run requires `soffice` and not `ffmpeg`; an `epub` run
  names `ebook-convert`; an `md` run names `pandoc`.
- Manual, per kind: one real file of each of the thirteen kinds through
  `source:{ext}/` to `destination:{pdf,txt,csv}/`, opening every output.
- **Gating this ADR's Status**: `ebook-convert` and `pandoc` are absent from the
  authoring machine, so the `epub` and `md`/`rst` routes are entirely
  unverified; no `.pages`, `.key` or `.numbers` file exists on it either, so
  `libetonyek`'s fidelity on real iWork documents is unverified for the same
  reason ADR-0149 recorded for `.numbers`; and the four Debian package names
  are unverified. `Proposed` becomes `Accepted` only once both binaries are
  installed and one real file per kind has been converted and inspected. Kinds
  that fail are removed and refiled rather than shipped on the strength of the
  registry inspection alone.
