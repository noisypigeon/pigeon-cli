# ADR-0160: `bmp`, `eps`, `ics` and `vcf` input kinds for `job run transform`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-17.
- **Status**: Accepted. Implemented on 2026-10-17, with the Ghostscript leg
  of §3 checked on the real file (see the Amendment and Verification).

## Context

The source bucket has four more prefixes `transform` cannot take. Following
ADR-0159's practice, each was listed with `rclone lsf source:` and its real
content read with `rclone cat` before anything was designed:

| Prefix | Files | Actually is | Wanted as |
|---|---|---|---|
| `bmp/` | 7 (238 KiB) | Small Windows BMPs, 8-bit and 1-bit paletted. 4 are spelled `.BMP` | `.jpg` |
| `eps/` | 1 (2 MiB) | A **DOS-binary EPS** (`C5 D0 D3 C6` magic): Illustrator 26.5 PostScript plus an embedded TIFF preview, `%%BoundingBox: 0 0 577 457` | `.svg` |
| `ics/` | 539 (3.8 MiB) | iCalendar. Mostly invites saved from email, but some are whole exported calendars | `.txt` |
| `vcf/` | 33 (811 KiB) | vCard. One file, `All Contacts.vcf`, holds **679 cards** | `.txt` |

Three findings shaped the design more than the format list did.

**LibreOffice turns this EPS into an empty SVG, and exits 0.** The route
`ai` already takes (`soffice --convert-to svg`, ADR-0159 §5) was run on the
real file. It wrote a 4,504-byte SVG on a 215.9 × 279.4 mm Letter page. Its
only content is one `<rect class="BoundingBox" fill="none" stroke="none">`.
There is no artwork, not even the embedded TIFF preview. The ten `<path>`
elements in it are LibreOffice's bullet-character templates, which it puts
in every SVG it writes. `soffice::convert`'s check is "a non-empty file where
the next stage expects one" (ADR-0149, adopted because soffice's exit code
can't be trusted). This file passes that check, so without a different route
`eps` would publish blank pages that report as successes.

**The calendar and contact files are noisy, and some are empty.** Across the
539 `.ics` files there are 6,202 `VEVENT`s, 2 `VTODO`s, 50 `VALARM`s, 242
`VTIMEZONE` blocks, 2,100 `RRULE`s and 1,363 `ATTENDEE`s. Most `DTSTART`s
carry a `TZID` (3,363 Toronto, 313 Vancouver, 108 Berlin, and others),
2,587 are bare UTC or floating times, and 107 are all-day `VALUE=DATE`s.
**Eight are 142-byte calendars with no component at all**: an airline's
ticket emails attach a `VCALENDAR` holding only `PRODID`, `VERSION`,
`CALSCALE` and `UID`. (Three more empty calendars turned up during
implementation; see the Amendment.) On the vCard side, 734 cards are version 3.0 and **one
is 2.1**. Apple's cards carry long base64 `itemN.X-ADDRESSING-GRAMMAR` blobs,
and 19 lines across the prefix are `PHOTO`, `ENCODING=b` or
`QUOTED-PRINTABLE`. In `All Contacts.vcf`, 655 of 679 cards have an `EMAIL`,
449 an `FN`, and 25 a `TEL`.

**A `.vcf` can hold hundreds of contacts.** That looks like ADR-0149's
spreadsheets, where one input became several outputs. It is not handled that
way here (§4).

## Decision

### 1. Four kinds

`InputFileType` gains `Bmp`, `Eps`, `Ics` and `Vcf`, taking it from 40 kinds
to 44. Each has one spelling, following `extensions()`'s rule that an alias
is only added once a real run misses files. The `.BMP` files are already
covered, because extension matching is case-insensitive (`matches_extension`,
and `--ignore-case` on the pull). Destinations come from ADR-0138's prefix
inference:

| Kind | Output | Default destination | Converter |
|---|---|---|---|
| bmp | `.jpg`, transparency flattened onto white | `destination:jpg/` | `ffmpeg` |
| eps | `.svg`, cropped to its bounding box | `destination:svg/` | `gs`, then `soffice` |
| ics | `.txt`, a readable rendering | `destination:txt/` | in-process |
| vcf | `.txt`, a readable rendering | `destination:txt/` | in-process |

### 2. `bmp` to JPEG, flattened onto white

`bmp` uses ADR-0159's `media::transcode_to_jpg_flattened`, with the same
full-size guard and the same retry policy as `avif`/`tif`/`webp`
(`retry_transcode_unless_fatal`). The sampled files are paletted and have no
alpha, but 32-bit `BI_BITFIELDS` BMPs can carry it. `bmp` joins the
flattening kinds rather than `png`/`heic`'s black-alpha path. ADR-0159 §2
kept `png`/`heic` unflattened only because they already had outputs in the
bucket, and `bmp` has none.

### 3. `eps` to SVG through Ghostscript

Given the Context's finding, the EPS goes through Ghostscript first, then
through the PDF-to-SVG route `ai` already uses:

1. `gs -dSAFER -dBATCH -dNOPAUSE -dEPSCrop -sDEVICE=pdfwrite -o <scratch>/<stem>.pdf <input>`
2. `documents::to_svg` on that PDF, which is `soffice::convert(…, "svg", …)`
   with ADR-0158's URL-encoded, kill-on-timeout runner. An `.ai` is a PDF, so
   this is the `ai` path with one step in front of it.

- **`-dSAFER` is load-bearing.** An EPS is a PostScript program from an
  untrusted bucket. Without `-dSAFER` it can read and write files on the
  host. The flag is passed explicitly, not left to whichever default the
  installed Ghostscript version ships.
- **`-dEPSCrop`** sizes the PDF page to the EPS bounding box, so the SVG is
  the drawing's own size. ADR-0159's `.emz` gets an uncropped Letter page
  instead.
- **The Ghostscript run gets a timeout and is killed on drop.** It is a
  program run on untrusted input, so it gets the same handling ADR-0158 gave
  soffice. A hung interpreter must not hold a concurrency slot forever.
- **The output gets one more check: it must have drawn something.** soffice
  exiting 0 with a non-empty file is exactly the case the Context measured
  coming back blank. So `eps_to_svg` refuses an SVG that contains no
  drawing element (`path`, `image`, `text`, `polygon`, `polyline`, `rect`,
  `circle`, `ellipse` or `line`) outside the `<defs>` blocks and the
  `BoundingBox` rect. This is non-retryable but **not** wrong-input-kind, so
  the local copy is kept for diagnosis (ADR-0143). The empty SVG from the
  Context has no such element, so this check would fail it. That was read
  from the file, and is to be pinned by a test (Verification).
- Not retried. A converter fails the same way on the same file every time
  (ADR-0149, ADR-0154).
- Ghostscript reads the DOS-binary EPS header itself. Verified on the real
  file during implementation.

`converter_for` gains `Converter::GhostscriptThenSoffice` for `eps`, with a
new `documents::check_ghostscript_available`. Its preflight checks both
binaries, since `eps` needs both. `converter_for` stays exhaustive, per
ADR-0154 §5. The Dockerfile adds `ghostscript` to its apt line.

### 4. `ics` and `vcf` to readable text, in a new `transform/vobject.rs`

Both formats are vObject content lines. `vobject.rs` has one parser shared by
both. No crate is added: the parsing is small, and the rendering, which is
where the real work is, would be written either way.

**Parsing.**

- Unfold continuation lines: a line break followed by a space or tab
  (RFC 5545 §3.1, RFC 6350 §3.2). Both CRLF and bare LF line endings are
  accepted, as is a missing final newline.
- Split each line into `NAME;PARAMS:VALUE`. A `:` or `;` inside a quoted
  parameter value does not split the line.
- Strip Apple's `itemN.` group prefix, keeping the `itemN.X-ABLabel` that
  shares the prefix as that property's label.
- Unescape `\n`, `\N`, `\,`, `\;` and `\\`.
- **vCard 2.1 `ENCODING=QUOTED-PRINTABLE`** values are decoded (`CHARSET`
  respected, UTF-8 by default), including 2.1's `=` soft line breaks, which
  it uses instead of space-folding. The one 2.1 card found would otherwise
  render as `=C3=A9`-style text.

**Input guard.** An `ics` input must contain `BEGIN:VCALENDAR`, and a `vcf`
input `BEGIN:VCARD`. Otherwise the file fails as wrong-input-kind, so
ADR-0143 drops the local copy. This follows ADR-0154 §2's `.key` guard:
check the input positively rather than trusting the extension.

**`ics` rendering.** The output starts with one header line: the calendar's
`X-WR-CALNAME` and `METHOD` when present. Then each `VEVENT` and `VTODO`
gets its own block, in file order:

- Summary
- When: `DTSTART`–`DTEND` (or `DUE`) as written, labelled with the `TZID`.
  An all-day `VALUE=DATE` is shown as a date. There is **no time-zone
  arithmetic**, because that needs the `VTIMEZONE` rules, and a wrong
  conversion is worse than an unconverted time with its zone named.
- Where
- Organizer: name and address
- Attendees: name, address and participation status
- Status
- Repeats: the raw `RRULE`
- URL
- Description

Dropped: `VTIMEZONE`, `VALARM`, `UID`, `DTSTAMP`, `SEQUENCE`,
`LAST-MODIFIED`, `CREATED` and every `X-` property. **A calendar with no
components still produces a file**: its header line and `No events.` The
eight empty airline calendars still land in the bucket as files, and are
neither failed nor silently skipped.

**`vcf` rendering.** Each card gets its own block, in file order:

- Name: `FN`, or `N` when `FN` is missing
- Organization and title
- Every `TEL`, `EMAIL`, `ADR` and `URL`, labelled with its `X-ABLabel` or
  `TYPE`
- Birthday
- Categories
- Note

Dropped: `PHOTO`, `LOGO`, `SOUND`, any `ENCODING=b`/`BASE64` value,
`X-ADDRESSING-GRAMMAR` and other `X-` properties, `PRODID`, `REV` and `UID`.
A card left with no renderable property is shown as `(no details)` rather
than disappearing, so the output's block count always equals the input's
card count.

**One input, one output.** `All Contacts.vcf` becomes one `.txt` holding 679
blocks, not 679 files. One output per input keeps `compute_destination_name`
a pure forward function, which ADR-0124's pre-pull skip relies on. It also
avoids ADR-0149's grouped naming, which was built for workbooks, where each
sheet is a document someone named. A contact card has no name that makes a
good filename, and a contacts export is read as one document. So neither
kind joins `is_spreadsheet()`, and `strict_stem` is not widened. Widening it
would duplicate existing objects rather than rename them, because this job
never deletes (ADR-0151).

Both run under `spawn_blocking` (ADR-0090), like `tsv_to_csv`. Both use
`Converter::None` and `Outcome::Transcoded`, and neither is retried. The
rendering is lossy on purpose, as `md`/`rst` already are (ADR-0154 §8): this
family's output is meant to be read.

These `.ics` files are email attachments that `email import` (ADR-0146)
would also deliver. Unlike ADR-0157's email-shaped Markdown, they are not
skipped. That Markdown was a rendering superseded by a better one, while
these are the original attachments.

## Consequences

- 580 sample files gain a route: 7 to jpg, 1 to svg and 572 to txt.
- **`eps` adds one new external binary, Ghostscript**, on the host and in
  the container image. Local development needs `brew install ghostscript`.
- `eps` cannot publish a blank SVG without failing the file, the case the
  Context measured.
- Calendar times stay in their source zones, labelled but not converted.
- Contact photos, alarm definitions and Apple's opaque metadata are not in
  the output. The source keeps them, since `transform` never touches the
  source (ADR-0112).

## Out of scope

- **Time-zone conversion of calendar times** to one zone. This needs the
  `VTIMEZONE` rules applied. It is permanent unless a real need appears,
  since a labelled local time is the more faithful rendering.
- **Expanding `RRULE` into occurrences.** The rule is shown as written.
- **Splitting a multi-card `.vcf` into one file per contact** (§4).
  Permanent: the reason is a decision, not a deferral.
- **Keeping contact photos**, for example as sibling `.jpg` files. That would
  make `vcf` multi-output, which §4 rejects.
- **Other PostScript kinds** (`.ps`, an EPS without a bounding box). Not in
  the bucket.

## Amends

- ADR-0154 §5: the converter preflight now has a kind that needs two binaries.
- ADR-0159 §2 and §5: `bmp` joins the flattening raster kinds, and `eps`
  joins `ai`'s soffice SVG route, behind a Ghostscript step and the
  drew-something check.

## Amendment (2026-10-17): implementation findings

Three things turned up while implementing this ADR.

- **The Ghostscript route works on the real EPS.** `gs` 10.08.0, then
  `soffice`, gives a 275 KB SVG with 457 drawn paths and no `<image>`. Its
  page is 203.28 × 161.12 mm, which is the file's `%%HiResBoundingBox` of
  576.23 × 456.73 pt. So `-dEPSCrop` used the high-resolution box, not the
  integer `%%BoundingBox` of 577 × 457 the Context quoted. Rendered and
  checked by eye: the full illustration, upright, nothing missing.
- **11 calendars are empty, not 8.** Besides the eight airline stubs,
  three Google Calendar exports (`Work Schedule.ics` and two copies of a
  school schedule, 177–196 bytes) hold a named `VCALENDAR` with no
  components. All eleven produce their `No events.` file, as §4 intends.
- **The interactive `--input-file-type` menu was missing ADR-0159's eleven
  kinds.** The menu is a string list the compiler cannot check against
  `InputFileType`, and ADR-0159 never touched it, so `avif` through `svg`
  could only be chosen by flag. This is a sixth unchecked place a new kind
  must be added, after the five ADR-0149 §7 and ADR-0154 §5 catalogued. The
  list is now a constant, `INTERACTIVE_INPUT_FILE_TYPES`, with all 44 kinds.
  A test enumerates every variant through an exhaustive `match`, so a new
  variant fails to compile until the test is updated, and the test then
  fails if the menu does not offer it.

## Verification

Done while landing the implementation:

- `mise run ci` clean: 639 tests.
- Pure tests for `vobject.rs`, with fixtures based on the sampled files.
  They cover:
  - a CRLF invite with a folded, escaped description, a `TZID` start and a
    `VALARM`/`VTIMEZONE` that must not appear in the output;
  - an empty calendar;
  - all-day, UTC, floating, mixed-zone and unparseable times, and a
    `DURATION`;
  - attendees, `RRULE` and a `VTODO`;
  - a fold inside a multi-byte character;
  - an Apple card with `itemN.` labels, a base64 blob and a `PHOTO`;
  - a multi-card export, including a card with nothing left to show;
  - a vCard 2.1 quoted-printable value with a soft line break and a
    Latin-1 fallback;
  - quoted parameters, unknown escapes, and both input guards, which are
    classified wrong-input-kind and non-retryable.
- Tests for the four kinds' parsing, output extensions and `outcome_for`,
  and the interactive-menu completeness test.
- ffmpeg-gated: 8-bit and 1-bit paletted `.BMP`s at 33 × 17 convert at
  their own size.
- The drew-something check refuses a fixture reduced from the measured
  empty SVG, and accepts one with artwork. A `linearGradient` is not
  counted as a `line`.
- gs-gated:
  - a 200 × 100 pt EPS converts to a 70.56 × 35.28 mm SVG (within soffice's
    0.01 mm rounding) that draws;
  - a non-PostScript `.eps` is refused as wrong-input-kind;
  - an EPS that tries to write a file on the host cannot, which pins
    `-dSAFER`.
- **Real files.** All four prefixes were pulled to scratch, 580 files, and
  run through the same converter functions the job calls, with 0 failures:
  - 7 of 7 BMPs convert at their own dimensions, from 30 to 544 px high;
  - the EPS is as described in the Amendment;
  - 539 of 539 `.ics` files render, and 6,204 `VEVENT`/`VTODO` blocks in
    gives 6,204 out. The largest is a 3,695-event export;
  - 33 of 33 `.vcf` files render, and 735 cards in gives 735 blocks out,
    679 of them in `All Contacts.vcf`;
  - no output is empty, and no base64, `X-ADDRESSING-GRAMMAR` or
    undecoded `=XX` text appears in any of them;
  - the one 2.1 card decodes to its real name, `Tom` followed by a
    zero-width space that the source genuinely contains.
- **Not done: a real `job run transform`.** `--destination` takes only a
  keyring bucket alias, and this job never deletes (ADR-0112), so a
  verification run would have written permanently into the real
  `destination:` bucket. The converters were run directly instead. The
  pull, placement and push legs are unchanged, and the existing tests cover
  them.
- **Dockerfile.** `ghostscript` installs on `debian:bookworm-slim` and
  reports version 10.00.0. **Not verified:** converting the real EPS with
  that older version inside the image. Per the project's proportionate
  approach to the image, no full image build was run.

Measured while writing this ADR:

- The four prefixes' counts, the content types, and the property tallies in
  the Context. Read-only, with `rclone lsf`, `rclone size` and `rclone cat`.
- `soffice --convert-to svg` on the real EPS: 4,504 bytes, exit 0, a Letter
  page with only a `BoundingBox` rect, no artwork.

Planned when the ADR was written. Every item was done, as recorded above:

- `mise run ci` clean.
- Pure tests for `vobject.rs`, with inline fixtures based on the sampled
  files: folded lines (including a fold inside a multi-byte character),
  escaped commas and semicolons, a `TZID` start time, an all-day event, an
  empty calendar, `itemN.` labels, a dropped base64 `PHOTO`, a vCard 2.1
  quoted-printable value with a soft line break, a card with no renderable
  property, and both input guards.
- Tests for kind parsing and `output_extension`, and the `outcome_for` table,
  for the four new kinds.
- An ffmpeg-gated `bmp` test, following
  `transcode_to_jpg_flattened_turns_transparency_white`'s skip-if-missing
  pattern.
- A test of the drew-something check against the empty SVG measured above,
  reduced to a fixture, and against a real one.
- A `gs`-gated `eps` test that skips when Ghostscript is absent.
- **Real files.** Run `job run transform` for each of the four kinds against
  `source:` and check:
  - 7 of 7 BMPs convert at their own dimensions;
  - the EPS's SVG contains vector drawing elements, not an `<image>`, and
    has the 577 × 457 pt page from its `%%BoundingBox`;
  - 539 of 539 `.ics` and 33 of 33 `.vcf` files produce a non-empty `.txt`;
  - `All Contacts.vcf`'s output has 679 blocks;
  - the eight empty calendars produce their `No events.` file.

  **Status moves to Accepted only once the EPS result has been checked by
  eye.** Anything not measured is recorded here as not verified. (Done:
  see above.)
- For the Dockerfile change, a `docker build` check only.
