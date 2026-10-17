//! Recognizes Markdown renderings of `.eml` messages (ADR-0157).
//!
//! `source:md/` holds a large share of these alongside real notes. The mail
//! is being redone from the source of truth by `job run email import`
//! (ADR-0146), so an `md` run skips them instead of converting them to
//! `.txt`. Their keys carry no signal, so this reads content.
//!
//! Four independent signal families; a file is email-shaped iff **at least
//! two** match. One family alone is too weak: a meeting note can carry
//! `from:`/`to:` frontmatter, and a blog draft can quote a header block.
//! Line-based throughout, with no YAML crate (ADR-0012's precedent).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of each end of a file `read_window` keeps. Frontmatter and the
/// header table sit at the top, the raw header block at the bottom.
pub(crate) const WINDOW_BYTES: u64 = 64 * 1024;

/// How many body lines (after any frontmatter) the header-table family
/// looks at.
const HEADER_TABLE_LINES: usize = 100;

/// Raw RFC 5322 header names that rarely appear outside an actual message.
const RAW_HEADER_NAMES: &[&str] = &[
    "message-id:",
    "received:",
    "return-path:",
    "mime-version:",
    "dkim-signature:",
    "delivered-to:",
    "arc-seal:",
    "x-gmail-labels:",
];

/// Which families matched. Recorded rather than collapsed to a `bool` so
/// the report can say *why* a file was skipped (ADR-0157 §4).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EmailSignals {
    pub frontmatter_headers: bool,
    pub provenance: bool,
    pub header_table: bool,
    pub raw_headers: bool,
}

impl EmailSignals {
    fn count(self) -> usize {
        [
            self.frontmatter_headers,
            self.provenance,
            self.header_table,
            self.raw_headers,
        ]
        .iter()
        .filter(|matched| **matched)
        .count()
    }

    /// Comma-joined family names, for a report row's `detail`.
    pub(crate) fn describe(self) -> String {
        [
            (self.frontmatter_headers, "frontmatter-headers"),
            (self.provenance, "provenance"),
            (self.header_table, "header-table"),
            (self.raw_headers, "raw-headers"),
        ]
        .iter()
        .filter(|(matched, _)| *matched)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// `Some` iff `text` is email-shaped: at least two families matched.
pub(crate) fn classify(text: &str) -> Option<EmailSignals> {
    let signals = signals(text);
    (signals.count() >= 2).then_some(signals)
}

/// Every family's verdict, regardless of the threshold.
pub(crate) fn signals(text: &str) -> EmailSignals {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (frontmatter, body) = split_frontmatter(text);

    let mut signals = EmailSignals::default();
    if let Some(frontmatter) = frontmatter {
        let keys: Vec<(&str, &str)> = frontmatter.iter().filter_map(|l| key_value(l)).collect();
        let has_key = |name: &str| keys.iter().any(|(key, _)| *key == name);

        let header_keys = ["from", "to", "cc", "subject"]
            .iter()
            .filter(|name| has_key(name))
            .count();
        signals.frontmatter_headers = has_key("sender_domain") || header_keys >= 2;

        signals.provenance = has_key("has_attachments")
            || has_key("uid")
            || has_key("also-in")
            || keys.iter().any(|(key, value)| {
                *key == "source" && unquote(value).to_ascii_lowercase().ends_with(".eml")
            })
            || frontmatter.iter().any(|line| has_taxonomy_tag(line));
    }

    let body_lines: Vec<&str> = body.lines().collect();
    signals.provenance |= body_lines.iter().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("original file") && lower.contains(".eml")
    });

    let mut labels_seen = [false; 4];
    for line in body_lines.iter().take(HEADER_TABLE_LINES) {
        let lower = line.trim().to_ascii_lowercase();
        for (seen, label) in labels_seen
            .iter_mut()
            .zip(["from", "to", "subject", "date"])
        {
            *seen |= is_header_label_line(&lower, label);
        }
    }
    signals.header_table = labels_seen.iter().filter(|seen| **seen).count() >= 3;

    let raw_names_seen = RAW_HEADER_NAMES
        .iter()
        .filter(|name| {
            text.lines()
                .any(|line| line.to_ascii_lowercase().starts_with(*name))
        })
        .count();
    signals.raw_headers = raw_names_seen >= 3;

    signals
}

/// Reads at most the first and last `WINDOW_BYTES` of `path` as lossy
/// UTF-8, bounding memory however large a rendered message body is.
pub(crate) fn read_window(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len <= 2 * WINDOW_BYTES {
        let mut bytes = Vec::with_capacity(len as usize);
        file.read_to_end(&mut bytes)?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let mut head = vec![0; WINDOW_BYTES as usize];
    file.read_exact(&mut head)?;
    file.seek(SeekFrom::End(-(WINDOW_BYTES as i64)))?;
    let mut tail = Vec::with_capacity(WINDOW_BYTES as usize);
    file.read_to_end(&mut tail)?;
    // A newline between the halves, so the tail's first (possibly partial)
    // line can never fuse with the head's last one.
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&head),
        String::from_utf8_lossy(&tail)
    ))
}

/// Splits a leading `---` ... `---` block off `text`. With no opening line,
/// or no closing one, there is no frontmatter and the whole text is body.
fn split_frontmatter(text: &str) -> (Option<Vec<&str>>, &str) {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return (None, text);
    };
    if first.trim() != "---" {
        return (None, text);
    }
    let mut consumed = first.len();
    let mut frontmatter = Vec::new();
    for line in lines {
        consumed += line.len();
        if line.trim() == "---" {
            return (Some(frontmatter), &text[consumed..]);
        }
        frontmatter.push(line.trim_end_matches(['\r', '\n']));
    }
    (None, text)
}

/// A top-level `key: value` frontmatter line. Indented lines and list
/// items belong to an enclosing key, not their own.
fn key_value(line: &str) -> Option<(&str, &str)> {
    if line.starts_with([' ', '\t', '-', '#']) {
        return None;
    }
    let (key, value) = line.split_once(':')?;
    Some((key.trim(), value.trim()))
}

fn unquote(value: &str) -> &str {
    value.trim_matches(|c| c == '"' || c == '\'')
}

/// pigeon's own ADR-0006 taxonomy tags, in either list or inline form.
fn has_taxonomy_tag(line: &str) -> bool {
    line.split(|c: char| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '"' | '\''))
        .any(|token| token.starts_with("mailbox/") || token.starts_with("identity/"))
}

/// `| **from** | ... |` or `**from:**` / `**from**:`, already lowercased.
fn is_header_label_line(lower: &str, label: &str) -> bool {
    if lower.starts_with('|') {
        let first_cell = lower
            .trim_start_matches('|')
            .split('|')
            .next()
            .unwrap_or("");
        return first_cell.trim() == format!("**{label}**");
    }
    lower.starts_with(&format!("**{label}:**")) || lower.starts_with(&format!("**{label}**:"))
}

/// The third-party converter's shape both prompting samples share,
/// trimmed and with placeholder addresses. Shared with `worker`'s tests.
#[cfg(test)]
pub(crate) const CONVERTER_SAMPLE: &str = "---
category:
tags: []
backlinks: []
status: unprocessed
sender_domain: example.com
has_attachments: false
---

# New Year Discount (Today only!)

| Field | Value |
|-------|-------|
| **Date** | 2022-01-02 02:25:41 |
| **From** | Sender <sender@example.com> |
| **To** | Recipient <recipient@example.com> |
| **CC** | _None_ |
| **Subject** | New Year Discount (Today only!) |
| **Original File** | `2022-01-02_New_Year_Discount.eml` |

## Attachments

_No attachments_

---

## Content

Happy New Year!

---

<details>
<summary>Raw Headers</summary>

```
Delivered-To: recipient@example.com
Received: by 2002:a9d:6209:0:0:0:0:0 with SMTP id g9csp8985691otj;
Return-Path: <bounces@example.com>
DKIM-Signature: v=1; a=rsa-sha1; c=relaxed/relaxed; s=cm; d=example.com;
Subject: New Year Discount (Today only!)
MIME-Version: 1.0
Message-ID: <cm.0225418681885@example.com>
```

</details>
";

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_FOUR: EmailSignals = EmailSignals {
        frontmatter_headers: true,
        provenance: true,
        header_table: true,
        raw_headers: true,
    };

    #[test]
    fn converter_sample_matches_every_family() {
        assert_eq!(classify(CONVERTER_SAMPLE), Some(ALL_FOUR));
    }

    #[test]
    fn converter_sample_without_raw_headers_still_matches() {
        let truncated = CONVERTER_SAMPLE.split("<details>").next().unwrap();
        let signals = classify(truncated).expect("email-shaped");
        assert!(!signals.raw_headers);
        assert!(signals.frontmatter_headers && signals.provenance && signals.header_table);
    }

    #[test]
    fn adr_0006_shape_matches() {
        let text = "---
from: \"Jane Doe <jane.doe@example.com>\"
to: \"first.last@example.com\"
subject: \"Hello, World!\"
date: 2024-01-26T09:15:00+00:00
tags:
  - mailbox/inbox
  - identity/first-last
  - sender/example-com
  - year/2024
attachments:
  - attachments/2024-01-26-hello-world-bingo-sheet.pdf
source: ../sunk/inbox/482.eml
---

Hi there.
";
        assert_eq!(
            classify(text),
            Some(EmailSignals {
                frontmatter_headers: true,
                provenance: true,
                header_table: false,
                raw_headers: false,
            })
        );
    }

    #[test]
    fn adr_0007_shape_matches_through_uid() {
        let text = "---
from: \"Jane Doe <jane.doe@example.com>\"
subject: \"Hello\"
tags: [year/2024]
uid: 482
---

Body.
";
        let signals = classify(text).expect("email-shaped");
        assert!(signals.frontmatter_headers && signals.provenance);
    }

    #[test]
    fn plain_note_is_not_email_shaped() {
        let text = "---
title: Groceries
date: 2024-03-01
tags: [home]
---

# Groceries

- eggs
- flour
";
        assert_eq!(classify(text), None);
        assert_eq!(signals(text), EmailSignals::default());
    }

    #[test]
    fn from_and_to_frontmatter_alone_is_one_family() {
        let text = "---
from: Alice
to: Bob
date: 2024-03-01
---

Meeting notes: ship it.
";
        let signals = signals(text);
        assert!(signals.frontmatter_headers);
        assert_eq!(classify(text), None);
    }

    #[test]
    fn quoted_header_block_alone_is_one_family() {
        let text = "# How DKIM works

```
Received: from mx.example.com
DKIM-Signature: v=1; a=rsa-sha256; d=example.com
Message-ID: <abc@example.com>
MIME-Version: 1.0
```

The signature covers the listed headers.
";
        let signals = signals(text);
        assert!(signals.raw_headers);
        assert_eq!(classify(text), None);
    }

    #[test]
    fn travel_table_is_not_a_header_table() {
        let text = "# Trip

| From | To | Date |
|------|----|------|
| Paris | Lyon | 2024-05-01 |
";
        assert_eq!(signals(text), EmailSignals::default());
    }

    #[test]
    fn identity_tag_alone_is_one_family() {
        let text = "---
tags: [identity/work]
---

Notes about my work identity.
";
        let signals = signals(text);
        assert!(signals.provenance);
        assert_eq!(classify(text), None);
    }

    #[test]
    fn empty_input_is_not_email_shaped() {
        assert_eq!(signals(""), EmailSignals::default());
    }

    #[test]
    fn unclosed_frontmatter_is_body() {
        let text = "---
from: Alice
subject: hello
";
        assert!(!signals(text).frontmatter_headers);
        assert_eq!(classify(text), None);
    }

    #[test]
    fn bold_colon_header_lines_count() {
        let text = "**From:** a@example.com\n**To:** b@example.com\n**Subject:** hi\n\nOriginal file: msg.eml\n";
        let signals = classify(text).expect("email-shaped");
        assert!(signals.header_table && signals.provenance);
    }

    #[test]
    fn describe_names_matched_families_in_order() {
        assert_eq!(
            ALL_FOUR.describe(),
            "frontmatter-headers, provenance, header-table, raw-headers"
        );
        assert_eq!(EmailSignals::default().describe(), "");
    }

    #[test]
    fn read_window_finds_raw_headers_past_a_large_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.md");
        let head = CONVERTER_SAMPLE.split("<details>").next().unwrap();
        let tail = &CONVERTER_SAMPLE[CONVERTER_SAMPLE.find("<details>").unwrap()..];
        let filler = "lorem ipsum dolor sit amet\n".repeat(10_000); // ~270 KiB
        std::fs::write(&path, format!("{head}{filler}{tail}")).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 2 * WINDOW_BYTES);

        let window = read_window(&path).unwrap();
        assert!((window.len() as u64) < 2 * WINDOW_BYTES + 2);
        assert_eq!(classify(&window), Some(ALL_FOUR));
    }

    #[test]
    fn read_window_reads_a_small_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.md");
        std::fs::write(&path, CONVERTER_SAMPLE).unwrap();
        assert_eq!(read_window(&path).unwrap(), CONVERTER_SAMPLE);
    }
}
