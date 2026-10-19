//! Full-resolution PNG export of a Procreate document (CLI-0159).
//!
//! A `.procreate` file is a zip. Its `Document.archive` is an
//! NSKeyedArchiver binary plist describing the canvas, and every layer's
//! pixels live under `<layer UUID>/<col>~<row>.chunk` as LZO1X-compressed
//! tiles of premultiplied RGBA. Crucially, the archive's root also names a
//! **`composite`** layer: a flattened image Procreate maintains itself. So
//! exporting at full resolution means reassembling that one layer, not
//! reimplementing Procreate's blend modes, masks and clipping.
//!
//! Measured on real documents before this was written:
//!
//! - tiles are stored **bottom-up**, so the reassembled image is flipped
//!   vertically;
//! - the document's `orientation` then rotates it: 3 is 90° counter-
//!   clockwise and 4 is 90° clockwise, both confirmed against the
//!   document's own thumbnail; 1 (none) and 2 (180°) complete the set;
//! - tiles are sparse -- an absent one is empty canvas -- and the last row
//!   and column are stored cropped to the canvas edge, not padded.
//!
//! Because those are observations rather than a published format, every
//! export is **verified** against the `QuickLook/Thumbnail.png` Procreate
//! writes into the same file before it is accepted. A stale composite or a
//! wrong orientation fails the file instead of shipping a wrong image.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;

use plist::Value;

use super::lzo1x;
use super::media::{NOT_A_PROCREATE_SIGNATURE, PROCREATE_UNVERIFIED_SIGNATURE};

/// The largest canvas side this accepts. Procreate's own maximum is far
/// below this; it exists so a crafted `size` cannot make the canvas
/// allocation unbounded. 16384 x 16384 RGBA is 1 GiB.
const MAX_SIDE: u32 = 16_384;

/// Mean absolute RGB difference, in 0-255 units, above which an export is
/// refused as not matching its thumbnail.
///
/// Calibrated in CLI-0159 on five real documents: every correct export
/// scored 0.01-1.66, and every wrong orientation of the same documents
/// scored 13.27-44.72 (the lowest a mostly-white drawing). 6 sits well
/// clear of both.
const MAX_THUMBNAIL_DIFFERENCE: f64 = 6.0;

const THUMBNAIL_PATH: &str = "QuickLook/Thumbnail.png";

/// An RGBA8 image, straight (not premultiplied) alpha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

fn unreadable(detail: impl std::fmt::Display) -> String {
    format!("{detail} -- {NOT_A_PROCREATE_SIGNATURE}")
}

/// Converts `input` to a PNG at `output`. Blocking: call it from
/// `spawn_blocking` (CLI-0090).
pub(crate) fn export_png(input: &Path, output: &Path) -> Result<(), String> {
    let file = std::fs::File::open(input)
        .map_err(|err| format!("failed to open {}: {err}", input.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|err| unreadable(format!("{} is not a zip: {err}", input.display())))?;

    let document = Document::parse(&read_entry(&mut zip, "Document.archive")?)?;
    let composite = assemble(&mut zip, &document)?;
    let oriented = orient(flip_vertical(composite), document.orientation)?;

    let thumbnail = decode_png(&read_entry(&mut zip, THUMBNAIL_PATH)?).map_err(|err| {
        format!(
            "{}: its thumbnail could not be decoded ({err}), so the export cannot be \
             verified -- {PROCREATE_UNVERIFIED_SIGNATURE}",
            input.display()
        )
    })?;
    verify_against_thumbnail(&oriented, &thumbnail)
        .map_err(|err| format!("{}: {err}", input.display()))?;

    std::fs::write(output, encode_png(&oriented)?)
        .map_err(|err| format!("failed to write {}: {err}", output.display()))
}

fn read_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, String> {
    let mut entry = zip
        .by_name(name)
        .map_err(|err| unreadable(format!("missing {name}: {err}")))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|err| unreadable(format!("failed to read {name}: {err}")))?;
    Ok(bytes)
}

/// The handful of fields this needs out of `Document.archive`.
#[derive(Debug, PartialEq, Eq)]
struct Document {
    width: u32,
    height: u32,
    tile_size: u32,
    orientation: i64,
    composite_uuid: String,
}

impl Document {
    fn parse(archive: &[u8]) -> Result<Self, String> {
        let value = Value::from_reader(Cursor::new(archive))
            .map_err(|err| unreadable(format!("Document.archive is not a plist: {err}")))?;
        let keyed = KeyedArchive::new(&value)?;
        let root = keyed.dict(keyed.top("root")?)?;

        let (width, height) = parse_size(
            keyed
                .resolve(
                    root.get("size")
                        .ok_or_else(|| unreadable("no canvas size"))?,
                )?
                .as_string()
                .ok_or_else(|| unreadable("canvas size is not a string"))?,
        )?;
        let tile_size = root
            .get("tileSize")
            .and_then(Value::as_unsigned_integer)
            .ok_or_else(|| unreadable("no tile size"))? as u32;
        if !(16..=4096).contains(&tile_size) {
            return Err(unreadable(format!("implausible tile size {tile_size}")));
        }
        let orientation = root
            .get("orientation")
            .and_then(Value::as_signed_integer)
            .unwrap_or(1);
        let composite = keyed.dict(
            keyed.resolve(
                root.get("composite")
                    .ok_or_else(|| unreadable("no composite layer"))?,
            )?,
        )?;
        let composite_uuid = keyed
            .resolve(
                composite
                    .get("UUID")
                    .ok_or_else(|| unreadable("composite layer has no UUID"))?,
            )?
            .as_string()
            .ok_or_else(|| unreadable("composite UUID is not a string"))?
            .to_string();
        // The UUID becomes part of a zip path prefix, so refuse anything
        // that is not plainly an identifier.
        if composite_uuid.is_empty()
            || !composite_uuid
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(unreadable("composite UUID is malformed"));
        }

        Ok(Document {
            width,
            height,
            tile_size,
            orientation,
            composite_uuid,
        })
    }
}

/// Just enough NSKeyedArchiver to follow references: every object lives in
/// `$objects`, and a reference to one is a plist UID indexing into it.
struct KeyedArchive<'a> {
    top: &'a plist::Dictionary,
    objects: &'a [Value],
}

impl<'a> KeyedArchive<'a> {
    fn new(value: &'a Value) -> Result<Self, String> {
        let dict = value
            .as_dictionary()
            .ok_or_else(|| unreadable("Document.archive is not a keyed archive"))?;
        let top = dict
            .get("$top")
            .and_then(Value::as_dictionary)
            .ok_or_else(|| unreadable("Document.archive has no $top"))?;
        let objects = dict
            .get("$objects")
            .and_then(Value::as_array)
            .ok_or_else(|| unreadable("Document.archive has no $objects"))?;
        Ok(Self { top, objects })
    }

    fn top(&self, key: &str) -> Result<&'a Value, String> {
        self.resolve(
            self.top
                .get(key)
                .ok_or_else(|| unreadable(format!("Document.archive has no $top.{key}")))?,
        )
    }

    /// Follows a UID reference; any other value is returned as is.
    fn resolve(&self, value: &'a Value) -> Result<&'a Value, String> {
        match value.as_uid() {
            Some(uid) => self
                .objects
                .get(uid.get() as usize)
                .ok_or_else(|| unreadable("dangling archive reference")),
            None => Ok(value),
        }
    }

    fn dict(&self, value: &'a Value) -> Result<&'a plist::Dictionary, String> {
        value
            .as_dictionary()
            .ok_or_else(|| unreadable("expected an archived object"))
    }
}

/// `"{3400, 4400}"` to `(3400, 4400)`.
fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let inner = text
        .trim()
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| unreadable(format!("canvas size {text:?} is malformed")))?;
    let mut parts = inner.split(',').map(|part| part.trim().parse::<f64>());
    let (Some(Ok(width)), Some(Ok(height)), None) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(unreadable(format!("canvas size {text:?} is malformed")));
    };
    let (width, height) = (width.round(), height.round());
    if !(1.0..=MAX_SIDE as f64).contains(&width) || !(1.0..=MAX_SIDE as f64).contains(&height) {
        return Err(unreadable(format!("canvas size {text:?} is out of range")));
    }
    Ok((width as u32, height as u32))
}

/// `<uuid>/<col>~<row>.chunk` to `(col, row)`.
fn tile_position(name: &str, uuid: &str) -> Option<(u32, u32)> {
    let rest = name.strip_prefix(uuid)?.strip_prefix('/')?;
    let (col, row) = rest.strip_suffix(".chunk")?.split_once('~')?;
    Some((col.parse().ok()?, row.parse().ok()?))
}

/// Reassembles the composite layer's tiles into one straight-alpha image,
/// still in storage order (bottom-up, unrotated).
fn assemble<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    document: &Document,
) -> Result<Image, String> {
    let (width, height, tile) = (document.width, document.height, document.tile_size);
    let mut pixels = vec![0u8; width as usize * height as usize * 4];

    let tiles: HashMap<String, (u32, u32)> = zip
        .file_names()
        .filter_map(|name| {
            tile_position(name, &document.composite_uuid).map(|pos| (name.to_string(), pos))
        })
        .collect();
    if tiles.is_empty() {
        return Err(unreadable("the composite layer has no tiles"));
    }

    for (name, (col, row)) in tiles {
        let (x0, y0) = (col as u64 * tile as u64, row as u64 * tile as u64);
        if x0 >= width as u64 || y0 >= height as u64 {
            return Err(unreadable(format!("tile {name} lies outside the canvas")));
        }
        // An edge tile is stored **cropped** to the canvas, not padded to
        // a full tile: on a 2732-high canvas the last row is 256 x 172.
        // Measured; the row stride is the cropped width.
        let (x0, y0) = (x0 as usize, y0 as usize);
        let tile_width = (tile as usize).min(width as usize - x0);
        let tile_height = (tile as usize).min(height as usize - y0);
        let tile_bytes = tile_width * tile_height * 4;
        let compressed = read_entry(zip, &name)?;
        let raw = lzo1x::decompress(&compressed, tile_bytes)
            .map_err(|err| unreadable(format!("tile {name}: {err}")))?;
        if raw.len() != tile_bytes {
            return Err(unreadable(format!(
                "tile {name} decompressed to {} bytes, expected {tile_bytes} \
                 ({tile_width}x{tile_height})",
                raw.len()
            )));
        }
        for ty in 0..tile_height {
            let src = &raw[ty * tile_width * 4..][..tile_width * 4];
            let dst = ((y0 + ty) * width as usize + x0) * 4;
            pixels[dst..dst + tile_width * 4].copy_from_slice(src);
        }
    }

    unpremultiply(&mut pixels);
    Ok(Image {
        width,
        height,
        pixels,
    })
}

/// Premultiplied RGBA to straight RGBA, which is what PNG stores.
fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
}

fn flip_vertical(mut image: Image) -> Image {
    let row = image.width as usize * 4;
    let height = image.height as usize;
    for y in 0..height / 2 {
        let (top, bottom) = image.pixels.split_at_mut((height - 1 - y) * row);
        top[y * row..(y + 1) * row].swap_with_slice(&mut bottom[..row]);
    }
    image
}

/// Rotates per Procreate's `orientation`, which follows UIKit's interface
/// orientation numbering.
fn orient(image: Image, orientation: i64) -> Result<Image, String> {
    match orientation {
        1 => Ok(image),
        2 => Ok(rotate(&image, Rotation::Half)),
        3 => Ok(rotate(&image, Rotation::CounterClockwise)),
        4 => Ok(rotate(&image, Rotation::Clockwise)),
        other => Err(unreadable(format!("unknown canvas orientation {other}"))),
    }
}

#[derive(Debug, Clone, Copy)]
enum Rotation {
    Clockwise,
    CounterClockwise,
    Half,
}

fn rotate(image: &Image, rotation: Rotation) -> Image {
    let (w, h) = (image.width as usize, image.height as usize);
    let (out_w, out_h) = match rotation {
        Rotation::Half => (w, h),
        Rotation::Clockwise | Rotation::CounterClockwise => (h, w),
    };
    let mut pixels = vec![0u8; image.pixels.len()];
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match rotation {
                Rotation::Clockwise => (h - 1 - y, x),
                Rotation::CounterClockwise => (y, w - 1 - x),
                Rotation::Half => (w - 1 - x, h - 1 - y),
            };
            let src = (y * w + x) * 4;
            let dst = (ny * out_w + nx) * 4;
            pixels[dst..dst + 4].copy_from_slice(&image.pixels[src..src + 4]);
        }
    }
    Image {
        width: out_w as u32,
        height: out_h as u32,
        pixels,
    }
}

fn decode_png(bytes: &[u8]) -> Result<Image, String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|err| err.to_string())?;
    let size = reader
        .output_buffer_size()
        .ok_or("thumbnail is too large to decode")?;
    let mut buffer = vec![0u8; size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|err| err.to_string())?;
    buffer.truncate(info.buffer_size());
    let pixels = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buffer.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::Indexed => return Err("unexpanded palette".to_string()),
    };
    Ok(Image {
        width: info.width,
        height: info.height,
        pixels,
    })
}

fn encode_png(image: &Image) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|err| format!("failed to encode PNG: {err}"))?;
    writer
        .write_image_data(&image.pixels)
        .map_err(|err| format!("failed to encode PNG: {err}"))?;
    writer
        .finish()
        .map_err(|err| format!("failed to encode PNG: {err}"))?;
    Ok(out)
}

/// One RGBA pixel composited onto white, for comparison: the thumbnail and
/// the composite may disagree about how transparency is stored, never
/// about what the picture looks like.
fn over_white(pixel: &[u8]) -> [f64; 3] {
    let alpha = pixel[3] as f64 / 255.0;
    [0, 1, 2].map(|c| pixel[c] as f64 * alpha + 255.0 * (1.0 - alpha))
}

/// Box-downscales `image` to `width` x `height`, averaging each source
/// pixel's composited-onto-white colour into the cell it falls in.
fn downscale(image: &Image, width: u32, height: u32) -> Vec<[f64; 3]> {
    let mut sums = vec![[0f64; 3]; (width * height) as usize];
    let mut counts = vec![0u32; (width * height) as usize];
    for y in 0..image.height {
        let cy = (y as u64 * height as u64 / image.height as u64) as u32;
        for x in 0..image.width {
            let cx = (x as u64 * width as u64 / image.width as u64) as u32;
            let index = (y * image.width + x) as usize * 4;
            let rgb = over_white(&image.pixels[index..index + 4]);
            let cell = (cy * width + cx) as usize;
            for c in 0..3 {
                sums[cell][c] += rgb[c];
            }
            counts[cell] += 1;
        }
    }
    sums.iter()
        .zip(counts)
        .map(|(sum, count)| sum.map(|value| value / count.max(1) as f64))
        .collect()
}

/// Mean absolute RGB difference between `image` scaled down to
/// `thumbnail`'s size and the thumbnail itself.
fn thumbnail_difference(image: &Image, thumbnail: &Image) -> f64 {
    let scaled = downscale(image, thumbnail.width, thumbnail.height);
    let total: f64 = scaled
        .iter()
        .zip(thumbnail.pixels.chunks_exact(4))
        .map(|(ours, theirs)| {
            let theirs = over_white(theirs);
            (0..3).map(|c| (ours[c] - theirs[c]).abs()).sum::<f64>()
        })
        .sum();
    total / (scaled.len() as f64 * 3.0)
}

fn verify_against_thumbnail(image: &Image, thumbnail: &Image) -> Result<(), String> {
    // The thumbnail is the canvas scaled to fit; its aspect ratio must match
    // the oriented export to within rounding, or the orientation is wrong.
    let expected_height = thumbnail.width as f64 * image.height as f64 / image.width as f64;
    if (expected_height - thumbnail.height as f64).abs() > 1.5 {
        return Err(format!(
            "export is {}x{} but the thumbnail is {}x{}, a different shape -- \
             {PROCREATE_UNVERIFIED_SIGNATURE}",
            image.width, image.height, thumbnail.width, thumbnail.height
        ));
    }
    let difference = thumbnail_difference(image, thumbnail);
    if difference > MAX_THUMBNAIL_DIFFERENCE {
        return Err(format!(
            "export differs from the thumbnail by {difference:.1} (limit \
             {MAX_THUMBNAIL_DIFFERENCE}) -- {PROCREATE_UNVERIFIED_SIGNATURE}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x3 image whose pixels are numbered 0..6 in row-major order, in the
    /// red channel.
    fn numbered() -> Image {
        Image {
            width: 2,
            height: 3,
            pixels: (0..6u8).flat_map(|n| [n, 0, 0, 255]).collect(),
        }
    }

    fn reds(image: &Image) -> Vec<u8> {
        image.pixels.chunks_exact(4).map(|p| p[0]).collect()
    }

    #[test]
    fn flip_vertical_reverses_row_order() {
        // 0 1 / 2 3 / 4 5  ->  4 5 / 2 3 / 0 1
        assert_eq!(reds(&flip_vertical(numbered())), [4, 5, 2, 3, 0, 1]);
    }

    #[test]
    fn rotations_match_their_names() {
        // 0 1 / 2 3 / 4 5 clockwise is 4 2 0 / 5 3 1.
        let cw = rotate(&numbered(), Rotation::Clockwise);
        assert_eq!((cw.width, cw.height), (3, 2));
        assert_eq!(reds(&cw), [4, 2, 0, 5, 3, 1]);
        // Counter-clockwise is 1 3 5 / 0 2 4.
        let ccw = rotate(&numbered(), Rotation::CounterClockwise);
        assert_eq!(reds(&ccw), [1, 3, 5, 0, 2, 4]);
        assert_eq!(
            reds(&rotate(&numbered(), Rotation::Half)),
            [5, 4, 3, 2, 1, 0]
        );
    }

    #[test]
    fn orientation_maps_procreates_numbers() {
        assert_eq!(orient(numbered(), 1).unwrap(), numbered());
        assert_eq!(
            orient(numbered(), 4).unwrap(),
            rotate(&numbered(), Rotation::Clockwise)
        );
        assert_eq!(
            orient(numbered(), 3).unwrap(),
            rotate(&numbered(), Rotation::CounterClockwise)
        );
        let err = orient(numbered(), 7).unwrap_err();
        assert!(
            super::super::media::is_wrong_input_kind_error(&err),
            "{err}"
        );
    }

    #[test]
    fn unpremultiply_restores_straight_alpha() {
        // 50% alpha red stored premultiplied as 128.
        let mut pixels = vec![128, 0, 0, 128, 10, 20, 30, 255, 9, 9, 9, 0];
        unpremultiply(&mut pixels);
        assert_eq!(&pixels[..4], &[255, 0, 0, 128]);
        assert_eq!(&pixels[4..8], &[10, 20, 30, 255], "opaque untouched");
        assert_eq!(&pixels[8..], &[9, 9, 9, 0], "transparent untouched");
    }

    #[test]
    fn parse_size_reads_the_archived_string() {
        assert_eq!(parse_size("{3400, 4400}").unwrap(), (3400, 4400));
        assert_eq!(parse_size("{2048,2732}").unwrap(), (2048, 2732));
        assert!(parse_size("{0, 10}").is_err());
        assert!(parse_size("{99999, 10}").is_err());
        assert!(parse_size("3400x4400").is_err());
    }

    #[test]
    fn tile_position_only_matches_the_composite_layer() {
        let uuid = "D2A91945-ABCD";
        assert_eq!(
            tile_position("D2A91945-ABCD/3~12.chunk", uuid),
            Some((3, 12))
        );
        assert_eq!(tile_position("OTHER/3~12.chunk", uuid), None);
        assert_eq!(tile_position("D2A91945-ABCD/thumb.png", uuid), None);
    }

    /// A 4x2 gradient and a thumbnail of it: the true orientation passes and
    /// a rotated export is refused.
    #[test]
    fn verification_accepts_a_match_and_refuses_a_wrong_shape_or_picture() {
        let image = Image {
            width: 4,
            height: 2,
            pixels: (0..8u32)
                .flat_map(|n| [(n * 30) as u8, 0, 0, 255])
                .collect(),
        };
        let thumbnail = Image {
            width: 2,
            height: 1,
            pixels: downscale(&image, 2, 1)
                .iter()
                .flat_map(|rgb| [rgb[0] as u8, rgb[1] as u8, rgb[2] as u8, 255])
                .collect(),
        };
        verify_against_thumbnail(&image, &thumbnail).unwrap();

        let rotated = rotate(&image, Rotation::Clockwise);
        let err = verify_against_thumbnail(&rotated, &thumbnail).unwrap_err();
        assert!(err.contains("different shape"), "{err}");

        let mirrored = rotate(&image, Rotation::Half);
        let err = verify_against_thumbnail(&mirrored, &thumbnail).unwrap_err();
        assert!(err.contains(PROCREATE_UNVERIFIED_SIGNATURE), "{err}");
        assert!(!super::super::media::is_wrong_input_kind_error(&err));
    }

    #[test]
    fn png_round_trips() {
        let image = numbered();
        assert_eq!(decode_png(&encode_png(&image).unwrap()).unwrap(), image);
    }

    /// End to end on a synthetic document: a 16x8 canvas with 16px tiles,
    /// so its one tile is an edge tile stored cropped to 16x8, orientation
    /// 1, and a matching thumbnail. Built in-test so no real artwork is
    /// committed.
    #[test]
    fn export_png_reassembles_a_synthetic_document() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("art.procreate");
        let output = dir.path().join("art.png");
        let uuid = "11111111-2222-3333-4444-555555555555";

        // Stored bottom-up: tile row 0 is the *bottom* of the picture. Make
        // the bottom half blue and the top half red, opaque.
        let mut tile = Vec::new();
        for ty in 0..8 {
            for _ in 0..16 {
                tile.extend_from_slice(if ty < 4 {
                    &[0, 0, 255, 255]
                } else {
                    &[255, 0, 0, 255]
                });
            }
        }
        let expected = Image {
            width: 16,
            height: 8,
            pixels: (0..8)
                .flat_map(|y| {
                    let px: [u8; 4] = if y < 4 {
                        [255, 0, 0, 255]
                    } else {
                        [0, 0, 255, 255]
                    };
                    std::iter::repeat_n(px, 16).flatten()
                })
                .collect(),
        };

        write_synthetic(
            &input,
            uuid,
            "{16, 8}",
            1,
            &tile,
            &encode_png(&expected).unwrap(),
        );
        export_png(&input, &output).unwrap();
        assert_eq!(
            decode_png(&std::fs::read(&output).unwrap()).unwrap(),
            expected
        );

        // The same document with a thumbnail that disagrees is refused.
        let wrong = Image {
            pixels: expected.pixels.iter().map(|b| 255 - b).collect(),
            ..expected.clone()
        };
        write_synthetic(
            &input,
            uuid,
            "{16, 8}",
            1,
            &tile,
            &encode_png(&wrong).unwrap(),
        );
        let err = export_png(&input, &output).unwrap_err();
        assert!(err.contains(PROCREATE_UNVERIFIED_SIGNATURE), "{err}");
    }

    #[test]
    fn export_png_refuses_a_zip_without_a_document() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("x.procreate");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&input).unwrap());
        writer
            .start_file("hello.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut writer, b"hi").unwrap();
        writer.finish().unwrap();
        let err = export_png(&input, &dir.path().join("x.png")).unwrap_err();
        assert!(
            super::super::media::is_wrong_input_kind_error(&err),
            "{err}"
        );
        assert!(export_png(&dir.path().join("missing"), &dir.path().join("y")).is_err());
    }

    /// LZO1X literal-only encoding of `data` (at least 19 bytes): a `0`
    /// instruction is a long literal run of `zeros * 255 + 18 + last`
    /// bytes, where `last` must be non-zero, then the end marker.
    fn lzo_literal(data: &[u8]) -> Vec<u8> {
        let excess = data.len() - 18;
        let zeros = (excess - 1) / 255;
        let mut out = vec![0u8];
        out.extend(std::iter::repeat_n(0u8, zeros));
        out.push((excess - zeros * 255) as u8);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0x11, 0x00, 0x00]);
        out
    }

    fn write_synthetic(
        path: &Path,
        uuid: &str,
        size: &str,
        orientation: i64,
        tile: &[u8],
        thumbnail: &[u8],
    ) {
        use plist::{Dictionary, Uid};
        let mut root = Dictionary::new();
        root.insert("size".into(), Value::Uid(Uid::new(2)));
        root.insert("tileSize".into(), Value::Integer(16u64.into()));
        root.insert("orientation".into(), Value::Integer(orientation.into()));
        root.insert("composite".into(), Value::Uid(Uid::new(3)));
        let mut composite = Dictionary::new();
        composite.insert("UUID".into(), Value::Uid(Uid::new(4)));
        let objects = vec![
            Value::String("$null".into()),
            Value::Dictionary(root),
            Value::String(size.into()),
            Value::Dictionary(composite),
            Value::String(uuid.into()),
        ];
        let mut top = Dictionary::new();
        top.insert("root".into(), Value::Uid(Uid::new(1)));
        let mut archive = Dictionary::new();
        archive.insert("$top".into(), Value::Dictionary(top));
        archive.insert("$objects".into(), Value::Array(objects));
        let mut plist_bytes = Vec::new();
        Value::Dictionary(archive)
            .to_writer_binary(&mut plist_bytes)
            .unwrap();

        let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in [
            ("Document.archive".to_string(), plist_bytes),
            (format!("{uuid}/0~0.chunk"), lzo_literal(tile)),
            (THUMBNAIL_PATH.to_string(), thumbnail.to_vec()),
        ] {
            writer.start_file(name, options).unwrap();
            std::io::Write::write_all(&mut writer, &bytes).unwrap();
        }
        writer.finish().unwrap();
    }
}
