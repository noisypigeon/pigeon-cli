//! Document date extraction for non-media files (ADR-0074 §4): PDF's own
//! `/CreationDate` trailer entry, or OOXML's (docx/xlsx/pptx --
//! already-zip-container) `docProps/core.xml` `dcterms:created`, each
//! falling back to a plain-text scan of the document's content when no
//! metadata date is present. Legacy binary Office formats (pre-OOXML
//! `.doc`/`.xls`/`.ppt`) aren't parsed at all (ADR-0074 Out of scope) --
//! callers fall through to the object's own last-modified time for those.

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;

use super::date::{SimpleDate, parse_iso_date, scan_text_for_date};

/// PDF: reads `/CreationDate` from the trailer's `Info` dictionary; if
/// absent or malformed, extracts the first few pages' text and scans it for
/// a date-like substring.
pub(crate) fn pdf_date(bytes: &[u8]) -> Option<SimpleDate> {
    let document = lopdf::Document::load_mem(bytes).ok()?;
    if let Some(date) = pdf_creation_date(&document) {
        return Some(date);
    }
    let page_numbers: Vec<u32> = document.get_pages().keys().take(5).copied().collect();
    let text = document.extract_text(&page_numbers).ok()?;
    scan_text_for_date(&text)
}

fn pdf_creation_date(document: &lopdf::Document) -> Option<SimpleDate> {
    let info_object = document.trailer.get(b"Info").ok()?;
    let info = match info_object.as_reference() {
        Ok(id) => document.get_object(id).ok()?.as_dict().ok()?.clone(),
        Err(_) => info_object.as_dict().ok()?.clone(),
    };
    let raw = info.get(b"CreationDate").ok()?.as_str().ok()?;
    super::date::parse_pdf_date(raw)
}

/// OOXML (docx/xlsx/pptx): every one of these is itself a zip archive with
/// a fixed `docProps/core.xml` member carrying Dublin Core metadata,
/// including `<dcterms:created>`. Reads that one member's `created`
/// element; if absent, extracts `word/document.xml`'s (or the
/// spreadsheet/slide equivalent's) text nodes and scans them for a date.
pub(crate) fn ooxml_date(bytes: &[u8]) -> Option<SimpleDate> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;

    if let Some(date) = read_zip_member(&mut archive, "docProps/core.xml")
        .as_deref()
        .and_then(created_date_from_core_xml)
    {
        return Some(date);
    }

    for candidate in [
        "word/document.xml",
        "xl/sharedStrings.xml",
        "ppt/slides/slide1.xml",
    ] {
        if let Some(xml) = read_zip_member(&mut archive, candidate)
            && let Some(date) = scan_text_for_date(&text_content(&xml))
        {
            return Some(date);
        }
    }
    None
}

fn read_zip_member<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let mut entry = archive.by_name(name).ok()?;
    let mut contents = String::new();
    entry.read_to_string(&mut contents).ok()?;
    Some(contents)
}

/// Pulls the text of the first `<dcterms:created>` (or bare `<created>`,
/// depending on how the writer declared its namespace prefix -- matched by
/// local name only) element out of `docProps/core.xml`.
fn created_date_from_core_xml(xml: &str) -> Option<SimpleDate> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_created = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) if start.local_name().as_ref() == "created" => {
                in_created = true;
            }
            Ok(Event::Text(text)) if in_created => {
                if let Some(date) = parse_iso_date(text.as_ref()) {
                    return Some(date);
                }
                in_created = false;
            }
            Ok(Event::End(end)) if end.local_name().as_ref() == "created" => {
                in_created = false;
            }
            Ok(Event::Eof) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Concatenates every text node in an XML document, ignoring markup --
/// enough to scan for a date substring without a full schema-aware text
/// extractor (`word/document.xml`'s actual paragraph/run structure isn't
/// needed just to find a date).
fn text_content(xml: &str) -> String {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(bytes)) => {
                text.push_str(bytes.as_ref());
                text.push(' ');
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    #[test]
    fn pdf_date_returns_none_for_non_pdf_bytes() {
        assert_eq!(pdf_date(b"not a pdf"), None);
    }

    #[test]
    fn created_date_from_core_xml_reads_dcterms_created() {
        let xml = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="x" xmlns:dcterms="y">
  <dcterms:created>2024-01-26T09:15:00Z</dcterms:created>
</cp:coreProperties>"#;

        assert_eq!(
            created_date_from_core_xml(xml),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn created_date_from_core_xml_returns_none_when_absent() {
        let xml = r#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x"></cp:coreProperties>"#;
        assert_eq!(created_date_from_core_xml(xml), None);
    }

    #[test]
    fn ooxml_date_reads_core_xml_from_a_real_zip() {
        let core_xml = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="x" xmlns:dcterms="y">
  <dcterms:created>2024-01-26T09:15:00Z</dcterms:created>
</cp:coreProperties>"#;

        let mut buf = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut buf));
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            writer.start_file("docProps/core.xml", options).unwrap();
            writer.write_all(core_xml.as_bytes()).unwrap();
            writer.finish().unwrap();
        }

        assert_eq!(
            ooxml_date(&buf),
            Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26
            })
        );
    }

    #[test]
    fn ooxml_date_returns_none_for_non_zip_bytes() {
        assert_eq!(ooxml_date(b"not a zip"), None);
    }
}
