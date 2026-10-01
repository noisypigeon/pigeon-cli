//! A minimal year/month/day value plus the ad-hoc date parsers `media.rs`
//! (EXIF) and `documents.rs` (PDF/OOXML metadata, plain-text scan) each need
//! -- deliberately not the `time` crate's `Date` (built for
//! structured formatting of already-valid dates, not for tolerantly
//! picking one out of EXIF/PDF's own fixed-but-ad-hoc string formats).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SimpleDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl SimpleDate {
    fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            Some(SimpleDate { year, month, day })
        } else {
            None
        }
    }
}

impl fmt::Display for SimpleDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Parses EXIF's fixed `DateTimeOriginal` format, `"YYYY:MM:DD HH:MM:SS"`
/// (the EXIF spec always uses `:` in the date part, unlike ISO-8601) --
/// only the date portion is used, any trailing time/subsecond/timezone
/// content is ignored.
pub(crate) fn parse_exif_datetime(raw: &str) -> Option<SimpleDate> {
    let date_part = raw.split(' ').next()?;
    let mut segments = date_part.splitn(3, ':');
    let year = segments.next()?.parse().ok()?;
    let month = segments.next()?.parse().ok()?;
    let day = segments.next()?.parse().ok()?;
    SimpleDate::new(year, month, day)
}

/// Parses a PDF `/CreationDate` string value: `"D:YYYYMMDDHHmmSSOHH'mm'"`
/// per the PDF spec, with the `D:` prefix and everything after the first 8
/// digits optional/ignored -- some writers omit the prefix or the time
/// entirely.
pub(crate) fn parse_pdf_date(raw: &[u8]) -> Option<SimpleDate> {
    let text = std::str::from_utf8(raw).ok()?;
    let digits = text.strip_prefix("D:").unwrap_or(text);
    if digits.len() < 8 || !digits.as_bytes()[..8].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year = digits[0..4].parse().ok()?;
    let month = digits[4..6].parse().ok()?;
    let day = digits[6..8].parse().ok()?;
    SimpleDate::new(year, month, day)
}

/// Parses an OOXML `docProps/core.xml` `dcterms:created`/`dcterms:modified`
/// value: ISO-8601, e.g. `"2024-01-26T09:15:00Z"` -- only the date portion
/// is used.
pub(crate) fn parse_iso_date(raw: &str) -> Option<SimpleDate> {
    let date_part = raw.split('T').next()?;
    let mut segments = date_part.splitn(3, '-');
    let year = segments.next()?.parse().ok()?;
    let month = segments.next()?.parse().ok()?;
    let day = segments.next()?.parse().ok()?;
    SimpleDate::new(year, month, day)
}

/// Best-effort fallback for a document with no usable metadata: scans
/// extracted text for the first substring that looks like a date, trying a
/// handful of common written forms in order (ISO `YYYY-MM-DD`, US
/// `MM/DD/YYYY`, and long-form `Month D, YYYY`). Deliberately conservative --
/// a false negative (no date found, falls through to mtime/unknown) is far
/// preferable here to a false positive (an unrelated number misread as a
/// date).
pub(crate) fn scan_text_for_date(text: &str) -> Option<SimpleDate> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];

    for word in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '/') {
        if let Some(date) = parse_iso_date(word).or_else(|| parse_us_date(word)) {
            return Some(date);
        }
    }

    let lower = text.to_ascii_lowercase();
    for (index, month_name) in MONTHS.iter().enumerate() {
        if let Some(pos) = lower.find(month_name) {
            let tail = &text[pos + month_name.len()..];
            if let Some(date) = parse_long_form_date(tail, (index + 1) as u32) {
                return Some(date);
            }
        }
    }
    None
}

fn parse_us_date(word: &str) -> Option<SimpleDate> {
    let mut segments = word.splitn(3, '/');
    let month = segments.next()?.parse().ok()?;
    let day = segments.next()?.parse().ok()?;
    let year = segments.next()?.parse().ok()?;
    SimpleDate::new(year, month, day)
}

/// `tail` is the text immediately following a matched month name, e.g.
/// `" 26, 2024 ..."` for `"January 26, 2024"`.
fn parse_long_form_date(tail: &str, month: u32) -> Option<SimpleDate> {
    let digits: String = tail
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == ' ')
        .collect();
    let mut parts = digits
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|part| !part.is_empty());
    let day: u32 = parts.next()?.parse().ok()?;
    let year: i32 = parts.next()?.parse().ok()?;
    SimpleDate::new(year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exif_datetime_ignoring_time() {
        assert_eq!(
            parse_exif_datetime("2024:01:26 09:15:00"),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn rejects_malformed_exif_datetime() {
        assert_eq!(parse_exif_datetime("not a date"), None);
    }

    #[test]
    fn parses_pdf_date_with_d_prefix_and_timezone() {
        assert_eq!(
            parse_pdf_date(b"D:20240126091500+00'00'"),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn parses_pdf_date_without_prefix() {
        assert_eq!(
            parse_pdf_date(b"20240126"),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn parses_iso_date_with_time_and_zulu() {
        assert_eq!(
            parse_iso_date("2024-01-26T09:15:00Z"),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn scans_text_for_iso_date() {
        assert_eq!(
            scan_text_for_date("Report generated 2024-01-26 for review"),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn scans_text_for_us_slash_date() {
        assert_eq!(
            scan_text_for_date("Invoice date: 01/26/2024"),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn scans_text_for_long_form_date() {
        assert_eq!(
            scan_text_for_date("Signed on January 26, 2024 by both parties"),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn scan_text_returns_none_when_nothing_looks_like_a_date() {
        assert_eq!(scan_text_for_date("no dates anywhere in this text"), None);
    }

    #[test]
    fn display_formats_as_y_m_d() {
        assert_eq!(
            SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            }
            .to_string(),
            "2024-01-26"
        );
    }
}
