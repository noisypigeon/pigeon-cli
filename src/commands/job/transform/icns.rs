//! Picks the largest image out of a macOS `.icns` container (CLI-0159).
//!
//! ffmpeg cannot open the container at all, but every image a modern
//! `.icns` carries at a useful size is an embedded PNG or JPEG 2000 file,
//! which ffmpeg decodes fine. The container itself is trivial -- `icns`,
//! a big-endian total length, then entries of a 4-byte type, a 4-byte
//! length (header included) and the payload -- so it is read here rather
//! than through a crate.
//!
//! Only PNG and JPEG 2000 entries are considered. The legacy RLE bitmaps
//! (`is32`, `il32`, ...) and their separate alpha masks only exist at 16-
//! 128px and are always accompanied by a PNG entry in practice; a file
//! holding nothing else is refused rather than half-decoded.

use std::path::{Path, PathBuf};

use super::media::NOT_AN_ICNS_SIGNATURE;

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const JP2_MAGIC: &[u8] = b"\x00\x00\x00\x0cjP  \r\n\x87\n";
const J2K_CODESTREAM_MAGIC: &[u8] = b"\xff\x4f\xff\x51";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Png,
    Jpeg2000,
}

impl Encoding {
    fn extension(self) -> &'static str {
        match self {
            Encoding::Png => "png",
            Encoding::Jpeg2000 => "jp2",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry<'a> {
    pub kind: [u8; 4],
    pub encoding: Encoding,
    pub data: &'a [u8],
}

/// The nominal pixel width an entry type holds, for the types that may
/// carry PNG or JPEG 2000. `@2x` types count at their real pixel size.
fn nominal_size(kind: &[u8; 4]) -> Option<u32> {
    Some(match kind {
        b"icp4" => 16,
        b"icp5" | b"ic11" => 32,
        b"icp6" | b"ic12" => 64,
        b"ic07" => 128,
        b"ic08" | b"ic13" => 256,
        b"ic09" | b"ic14" => 512,
        b"ic10" => 1024,
        _ => return None,
    })
}

fn encoding_of(data: &[u8]) -> Option<Encoding> {
    if data.starts_with(PNG_MAGIC) {
        Some(Encoding::Png)
    } else if data.starts_with(JP2_MAGIC) || data.starts_with(J2K_CODESTREAM_MAGIC) {
        Some(Encoding::Jpeg2000)
    } else {
        None
    }
}

/// The largest PNG/JPEG 2000 entry in `bytes`: by nominal pixel size, then
/// by payload length (a fuller image of the same size compresses larger).
pub(crate) fn largest_entry(bytes: &[u8]) -> Result<Entry<'_>, String> {
    if bytes.len() < 8 || &bytes[..4] != b"icns" {
        return Err(format!(
            "does not start with an icns header -- {NOT_AN_ICNS_SIGNATURE}"
        ));
    }
    let mut best: Option<(u32, Entry<'_>)> = None;
    let mut offset = 8;
    while offset + 8 <= bytes.len() {
        let kind: [u8; 4] = bytes[offset..offset + 4].try_into().expect("4 bytes");
        let length =
            u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().expect("4 bytes")) as usize;
        if length < 8 || offset + length > bytes.len() {
            return Err(format!(
                "entry {} at byte {offset} overruns the file -- {NOT_AN_ICNS_SIGNATURE}",
                String::from_utf8_lossy(&kind)
            ));
        }
        let data = &bytes[offset + 8..offset + length];
        if let (Some(size), Some(encoding)) = (nominal_size(&kind), encoding_of(data)) {
            let better = match &best {
                None => true,
                Some((best_size, best_entry)) => {
                    (size, data.len()) > (*best_size, best_entry.data.len())
                }
            };
            if better {
                best = Some((
                    size,
                    Entry {
                        kind,
                        encoding,
                        data,
                    },
                ));
            }
        }
        offset += length;
    }
    best.map(|(_, entry)| entry)
        .ok_or_else(|| format!("has no PNG or JPEG 2000 image -- {NOT_AN_ICNS_SIGNATURE}"))
}

/// Writes `input`'s largest image into `work_dir` and returns its path,
/// named with the payload's real extension so ffmpeg probes it correctly.
pub(crate) fn extract_largest(input: &Path, work_dir: &Path) -> Result<PathBuf, String> {
    let bytes =
        std::fs::read(input).map_err(|err| format!("failed to read {}: {err}", input.display()))?;
    let entry = largest_entry(&bytes).map_err(|err| format!("{} {err}", input.display()))?;
    let path = work_dir.join(format!("icns-entry.{}", entry.encoding.extension()));
    std::fs::write(&path, entry.data)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = kind.to_vec();
        out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn container(entries: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = entries.concat();
        let mut out = b"icns".to_vec();
        out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn png(extra: usize) -> Vec<u8> {
        let mut data = PNG_MAGIC.to_vec();
        data.extend(std::iter::repeat_n(0u8, extra));
        data
    }

    #[test]
    fn picks_the_largest_nominal_size_not_the_largest_payload() {
        let bytes = container(&[
            entry(b"is32", &[0; 4000]),
            entry(b"ic08", &png(5000)),
            entry(b"ic10", &png(10)),
            entry(b"ic07", &png(9000)),
        ]);
        let best = largest_entry(&bytes).unwrap();
        assert_eq!(&best.kind, b"ic10");
        assert_eq!(best.encoding, Encoding::Png);
    }

    #[test]
    fn breaks_a_size_tie_by_payload_length_and_reads_jpeg_2000() {
        let mut jp2 = JP2_MAGIC.to_vec();
        jp2.extend([0u8; 50]);
        let bytes = container(&[entry(b"ic14", &png(10)), entry(b"ic09", &jp2)]);
        let best = largest_entry(&bytes).unwrap();
        assert_eq!(&best.kind, b"ic09");
        assert_eq!(best.encoding, Encoding::Jpeg2000);
    }

    #[test]
    fn refuses_a_file_with_only_legacy_bitmaps() {
        let bytes = container(&[entry(b"is32", &[1; 100]), entry(b"s8mk", &[1; 64])]);
        let err = largest_entry(&bytes).unwrap_err();
        assert!(err.contains("no PNG or JPEG 2000"), "{err}");
        assert!(super::super::media::is_wrong_input_kind_error(&err));
    }

    #[test]
    fn refuses_a_non_icns_and_a_truncated_entry() {
        assert!(largest_entry(b"\x89PNG\r\n\x1a\n").is_err());
        let mut bytes = container(&[entry(b"ic10", &png(100))]);
        bytes.truncate(bytes.len() - 10);
        let err = largest_entry(&bytes).unwrap_err();
        assert!(err.contains("overruns"), "{err}");
    }

    #[test]
    fn extract_largest_writes_the_payload_with_its_real_extension() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("app.icns");
        let payload = png(32);
        std::fs::write(&input, container(&[entry(b"ic12", &payload)])).unwrap();
        let path = extract_largest(&input, dir.path()).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(std::fs::read(path).unwrap(), payload);
    }
}
