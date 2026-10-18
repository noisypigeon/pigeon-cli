# ADR-0159: raster, icon, Procreate and vector input kinds for `job run transform`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-17.
- **Status**: Accepted.

## Context

The source bucket has prefixes for eleven image formats `transform` cannot
yet take. Each was listed with `rclone lsf source:`, every file was pulled
to scratch, and its real content identified with `file` before anything
was designed:

| Prefix | Files | Actually is | Existing ffmpeg path |
|---|---|---|---|
| `avif/` | 7 | AVIF, including tile-grid images | converts all 7 at full size (libdav1d) |
| `tif/`, `tiff/` | 2 + 15 | TIFF: LZW, 1-, 4- and 8-bit | converts all 17 |
| `webp/` | 152 | 145 still WebP, **4 animated**, **3 actually JPEG** | converts all 152 (first frame of the animated ones) |
| `ico/` | 1 | ICO with 32px and 16px 32-bit BMP images | converts |
| `icns/` | 8 | ICNS; 7 carry PNG entries `ic07`–`ic14`, 1 only a **JPEG 2000** `ic09` | **cannot open the container** ("Invalid data found") |
| `procreate/` | 3 | Procreate document (a zip) | n/a |
| `procreate 2/` | 2 | The same format. The extension is literally `procreate 2`, from macOS's duplicate naming, and each file differs from its `.procreate` namesake | n/a |
| `ai/` | 1 | PDF-compatible Illustrator file | n/a |
| `emz/` | 2 | gzip-compressed EMF | n/a |
| `svg/` | 4 | SVG | already the target |

Two findings shaped the design more than the format list did.

**Transparency.** JPEG has no alpha. With no instruction, ffmpeg simply
drops the channel, so a transparent icon converted to JPEG gets black
corners. Checked on a real `.icns`. That is also what the existing `png`
kind already does. Icons and Procreate artwork are routinely transparent.

**Procreate keeps its own flattened image.** A `.procreate` file is a zip.
`Document.archive` is an NSKeyedArchiver binary plist, and each layer's
pixels are LZO1X-compressed premultiplied-RGBA tiles under
`<layer UUID>/<col>~<row>.chunk`. The archive's root also names a
**`composite`** layer: the flattened picture, maintained by Procreate
itself. A full-resolution export therefore means reassembling **one**
layer, not reimplementing Procreate's blend modes, masks, clipping and
groups. The five real documents used:

- from a single layer group to 27 layers;
- hidden layers, clipping masks and partial opacity;
- a layer group;
- canvases from 2048² to **8000²**.

All five carry a composite. Measured by hand, before writing code:

- An LZO1X decoder ported from the Linux kernel's decompressed all 276
  chunks of one document. A full tile is exactly 256 × 256 × 4 bytes.
- **Edge tiles are stored cropped to the canvas, not padded.** A
  2732-high canvas's last row is 256 × 172. The first sample happened to
  be 2048², which divides evenly, so this only surfaced on the second.
- Tiles are **bottom-up**, so the reassembled image is flipped vertically.
  The document's `orientation` then rotates it, following UIKit's numbering.
- Tiles are **sparse**. An absent one is empty canvas: one document has
  40 tiles on a 14 × 18 grid.
- Each document embeds `QuickLook/Thumbnail.png`, the picture as Procreate
  shows it, scaled to fit 1024px.

## Decision

### 1. Eleven kinds, three output families

`InputFileType` gains `Avif`, `Tif`, `Tiff`, `Webp`, `Ico`, `Icns`,
`Procreate`, `Procreate2`, `Ai`, `Emz` and `Svg`, taking it from 29 kinds
to 40. Destinations come from ADR-0138's prefix inference as always:

| Kinds | Output | Default destination |
|---|---|---|
| avif, tif, tiff, webp | `.jpg`, transparency flattened onto white | `destination:jpg/` |
| ico, icns, procreate, procreate-2 | `.png`, transparency kept | `destination:png/` |
| ai, emz | `.svg` | `destination:svg/` |
| svg | `.svg`, copied through byte for byte (ADR-0112) | `destination:svg/` |

The split between jpg and png was the operator's choice. These formats were
asked for as JPEG, but the kinds that routinely carry transparency keep it
as PNG rather than lose it. The plain raster kinds stay JPEG, and those
that happen to be transparent are flattened onto white.

**`tif` and `tiff` are separate kinds.** They are one format, but the bucket
keeps them under two prefixes, and ADR-0138 infers the source prefix from
`canonical_extension()`. `Jpeg`'s two spellings share one prefix, so it
could be one kind. These cannot.

**`procreate-2` follows `Mp4Audio`'s precedent.** Its extension and prefix are
`procreate 2`, with a space, because that is the extension
`Path::extension` reports for `Art.procreate 2`. `rclone --include
'*.procreate 2'` was checked to match. It displays as `procreate-2` so it
can be typed unquoted, and `parse` accepts both spellings. Two documents
that share a stem across the two prefixes don't collide at
`destination:png/`, because `compute_destination_name` hashes the source
prefix together with the key.

### 2. Raster to JPEG, flattened onto white

`media::transcode_to_jpg_flattened` is the existing `transcode_to_jpg`, with
`-q:v 1`, 4:4:4 chroma, `-f mjpeg` and the full-size guard, plus one
filter. The filter composites the frame over an opaque white canvas of
the same size. An opaque image passes through unchanged (mean difference
≤ 2 in a test), and every one of the 176 avif/tif/tiff/webp samples kept its
exact dimensions. The two now share one `transcode_still` helper, so the
encoder settings cannot drift apart.

**`png` and `heic` keep their current behaviour.** Their already-pushed
outputs were encoded without flattening. A rerun never rewrites an
existing destination name, so changing them would leave the bucket
holding both renderings.

Recorded losses:

- An animated WebP keeps its first frame. Four are affected.
- A multi-page TIFF keeps page 1.
- The three `.webp` files that are really JPEG are decoded and re-encoded
  at the top quality setting rather than copied through. Sniffing content
  to copy them would make `outcome_for` per file instead of per kind,
  which three files don't justify.

### 3. Icons to PNG

`media::transcode_to_png` writes one frame as PNG with alpha, under the same
full-size guard. ffmpeg's default stream selection takes an ICO's largest
image.

For `.icns`, the new in-process `icns::largest_entry` reads the container
(four bytes of type and four of length per entry). It takes the PNG or
JPEG 2000 entry with the largest nominal size (`ic10`, 1024px, first),
breaking ties by payload length. It writes that entry to scratch with its
real extension, and ffmpeg converts it from there, so the full-size guard
probes a decodable file rather than the container. No crate was needed.

Legacy RLE bitmaps (`is32`, `il32`, …) only exist at 16–128px and are always
accompanied by a PNG entry in practice. A file holding nothing else is
refused as `is not a readable icns icon`, which is classified
wrong-input-kind (ADR-0143) rather than half-decoded.

Checked on the samples:

- the largest icons come out at 1024²;
- the JPEG 2000-only one at 512²;
- one whose largest entry is `ic13` at 256²;
- all of them RGBA.

ICO alpha was verified on a hand-built 32-bit BMP icon with mixed alpha.
ffmpeg's ICO *writer* zeroes BMP alpha bytes, and an all-zero BMP alpha
channel is read as "no alpha" by convention, so ffmpeg cannot produce
that fixture itself.

### 4. Procreate to a full-resolution PNG, verified against its own thumbnail

`procreate::export_png` runs in `spawn_blocking` (ADR-0090) with no
external binary:

1. **Parse.** Open the zip with the existing `zip` crate. Parse
   `Document.archive` with the existing `plist` crate, following
   NSKeyedArchiver UID references from `$top.root` to `size` (`"{W, H}"`),
   `tileSize`, `orientation` and `composite.UUID`. Anything missing or
   implausible fails the file as `is not a readable Procreate document`,
   which is wrong-input-kind. That covers a canvas side over 16384, a tile
   size outside 16–4096, and a UUID that isn't a plain identifier, since it
   becomes a zip path prefix.
2. **Reassemble.** Decompress each composite chunk with the new in-house
   `lzo1x.rs`, a port of the kernel's `lzo1x_decompress_safe`. Every read
   and back-reference is bounds-checked, and output is capped at the
   expected size, so a crafted chunk returns `Err` instead of panicking or
   allocating without limit. Each tile must decompress to exactly its
   cropped size. It is blitted into place, with absent tiles left
   transparent.
3. **Orient.** Un-premultiply alpha, flip vertically, then rotate by
   orientation. 1 needs no rotation, 2 is 180°, 3 is 90° counter-clockwise
   and 4 is 90° clockwise. Any other value fails the file.
4. **Verify.** The export must match the embedded thumbnail.
   - Its aspect ratio must agree to within 1.5 thumbnail pixels.
   - Scaled down to the thumbnail's size with both composited onto
     white, its mean absolute RGB difference must be at most **6** (out
     of 255).

   A mismatch fails the file as `does not match its own thumbnail`. That is
   non-retryable, since the same file decodes the same way every time, but
   it is **not** wrong-input-kind, so the local copy is kept for diagnosis.
   A missing or undecodable thumbnail fails the same way. This job's
   contract is that it verifies what it publishes (ADR-0130/0141), and
   with nothing to compare against there is no verification.
5. **Encode.** The new `png` crate writes the output. It also decodes the
   thumbnail and round-trips in a test.

The threshold was calibrated on the five documents, scoring each under all
four orientations:

| Document | Correct orientation | Lowest wrong orientation |
|---|---|---|
| 2048² | 0.01 | 21.67 |
| 2048 × 2732 | 0.43 | 13.27 |
| 3400 × 4400 | 0.27 | 13.67 |
| 3400 × 4400 (`procreate 2`) | 1.66 | 28.59 |
| 8000² | 0.19 | 41.58 |

Every correct export is under 2. Every wrong one is over 13, the lowest
being a mostly-white line drawing. 6 sits well clear of both. This also
confirms orientations 1, 3 and 4 on real documents. **Orientation 2 is
inferred** from the numbering, not observed. If it is wrong, the check
refuses the file rather than publishing it.

Results on the samples:

- all five exported at full canvas size: 2048², 2732 × 2048, 4400 × 3400 (twice) and 8000²;
- the 8000² export took 1.5 s;
- the one checked by eye is upright, with its hidden background left transparent.

Memory is width × height × 4 per file in flight, plus a second buffer
while rotating. That is 256 MB and 512 MB at peak for an 8000² canvas, so
very large documents want a lower `--concurrency`.

Not done, all made unnecessary by the composite:

- compositing layers or blend modes;
- reading the timelapse video;
- a thumbnail-only export (full resolution was the operator's choice).

### 5. Illustrator and EMZ to SVG

`documents::to_svg` is `soffice::convert(…, "svg", …)`, reusing ADR-0158's
URL-encoded, kill-on-timeout runner. LibreOffice Draw reads a modern `.ai`
through its PDF import and a gzip'd EMF directly. It is not retried,
following ADR-0154's reasoning for document kinds.

The `.ai` sample renders correctly at its own page size. Recorded
limitations:

- An `.emz` is **not cropped**. Draw places the drawing on its default
  Letter page, and the SVG keeps that page. Accepted for two files rather
  than adding Inkscape, which would crop with `--export-area-drawing`.
- A multi-artboard `.ai` exports its first page only.
- A pre-PDF, PostScript-era `.ai` fails as unreadable.

### 6. Preflight

`converter_for` stays exhaustive:

- avif, tif, tiff, webp, ico and icns need `ffmpeg`;
- ai and emz need `soffice`;
- svg, procreate and procreate-2 need nothing.

**No new binary and no Dockerfile change.** The libdav1d AV1 decoder and
the native JPEG 2000 decoder were both present in the ffmpeg build used
here. Debian's ffmpeg package normally includes them, but that is **not
verified** in the image.

## Consequences

- 197 sample files convert: 176 to jpg, 14 to png and 7 to svg, with 0
  failures across all eleven kinds.
- Three dependencies are added: `png`, plus in-house `lzo1x.rs` and
  `icns.rs` instead of crates.
- A wrong Procreate export cannot be published silently. The thumbnail
  check fails it instead.
- `destination:jpg/` now holds two flattening conventions: white for the
  new kinds, black for `png`/`heic`. This is stated in §2 and deliberate.

## Out of scope

- **Flattening `png`/`heic` onto white.** Changing it would split the bucket ([#127](https://github.com/noisypigeon/pigeon-cli/issues/127))
  between two renderings (§2). This would need a remediation decision of
  its own.
- **A hard pixel budget for huge Procreate canvases.** Procreate's own limits ([#128](https://github.com/noisypigeon/pigeon-cli/issues/128))
  keep real documents far below the 16384-side refusal. `--concurrency` is
  the lever for now.
- **Cropping `.emz`.** Permanent unless Inkscape is ever wanted for
  something else.
- **Procreate Dreams** (`.dream`), a different format.
- **AND-mask-only ICO transparency** (24-bit BMP images, whose
  transparency lives only in the 1-bit mask). Not present in the bucket,
  and not verified either way.

## Verification

- `mise run ci` clean.
- Pure tests:
  - **Kind parsing.** Every new spelling parses, round-trips through
    `name()` and maps to its prefix and output. `procreate 2` matches
    `Path::extension`'s output while displaying without the space.
  - **`icns::largest_entry`.** Size beats payload length, ties go to the
    larger payload, JPEG 2000 is detected, a legacy-only file is refused
    as wrong-input-kind, and truncation is reported.
  - **`lzo1x`.** It decodes literal runs, overlapping and zero-run
    matches, and rejects truncated input, a back-reference before the
    output's start, and an over-long output, all without panicking.
  - **Procreate image handling.** Flips, the three rotations and
    orientation mapping are checked on a numbered 2×3 image.
    Un-premultiplying, size parsing and tile-name parsing are tested, and
    the PNG round-trips.
  - **Thumbnail verification** accepts a match and refuses both a wrong
    shape and a mirrored picture.
  - **A synthetic Procreate document** is built in-test. It is a 16×8
    canvas whose one tile is a cropped edge tile, with an archive plist, an
    LZO chunk and a matching thumbnail. It exports to the expected
    pixels and is refused once its thumbnail is replaced. No real artwork
    is committed.
- ffmpeg-gated tests:
  - transparent red flattens to white, not black or red;
  - an opaque image is unchanged by flattening;
  - a mixed-alpha BMP icon becomes an RGBA PNG with exactly its 128 opaque
    and 128 transparent pixels.
- soffice-gated test: a one-page PDF renamed `.ai` becomes an SVG.
- Manual, done: every sample in the bucket went through `convert_one` into
  scratch, with nothing pushed, and 197 of 197 converted, with the
  dimensions and pixel formats above.
