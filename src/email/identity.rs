use serde::{Deserialize, Serialize};

use crate::email::provider::Provider;

/// A single authenticated email identity's non-secret metadata. The actual
/// secret (app/bridge password) lives in the OS keychain, keyed by `alias`
/// -- see `crate::keyring::credentials`. Metadata persistence itself lives
/// in `crate::keyring::store` (ADR-0022) -- this struct is kept here since
/// `email::sink`/`email::transform`/`job` are its main consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub alias: String,
    pub email: String,
    pub provider: Provider,
    pub host: String,
    pub port: u16,
}

/// Caps a sanitized segment's length so it can never blow past a
/// filesystem's per-component name limit (255 bytes on APFS/most Unix
/// filesystems) on its own -- e.g. an email subject line long enough to be
/// a whole paragraph. `sanitize_segment`'s output is always pure ASCII, so
/// a byte count is also a char count here.
const MAX_SEGMENT_LENGTH: usize = 100;

/// Sanitizes a single path/name segment: lowercase, non-alphanumeric runs
/// collapsed to a single hyphen, leading/trailing hyphens trimmed, capped to
/// `MAX_SEGMENT_LENGTH`. Shared by `sanitize_alias` and `sink`'s
/// per-mailbox directory naming.
pub fn sanitize_segment(input: &str) -> String {
    let mut segment = String::with_capacity(input.len());
    let mut last_was_hyphen = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            segment.push(ch.to_ascii_lowercase());
            last_was_hyphen = false;
        } else if !last_was_hyphen && !segment.is_empty() {
            segment.push('-');
            last_was_hyphen = true;
        }
    }
    segment.truncate(MAX_SEGMENT_LENGTH);
    if segment.ends_with('-') {
        segment.pop();
    }
    segment
}

/// Derives a default alias from an email address's local part, following
/// ADR-0001's file-naming scheme.
///
/// e.g. `first.last@example.com` -> `first-last`
pub fn sanitize_alias(email: &str) -> String {
    let local_part = email.split('@').next().unwrap_or(email);
    sanitize_segment(local_part)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_dotted_local_part() {
        assert_eq!(sanitize_alias("first.last@example.com"), "first-last");
    }

    #[test]
    fn sanitizes_plus_addressing() {
        assert_eq!(sanitize_alias("jane+work@example.com"), "jane-work");
    }

    #[test]
    fn sanitize_segment_truncates_long_input_with_no_trailing_hyphen() {
        let long_subject = "word ".repeat(50); // far more than MAX_SEGMENT_LENGTH once hyphenated
        let segment = sanitize_segment(&long_subject);
        assert!(segment.len() <= MAX_SEGMENT_LENGTH);
        assert!(!segment.ends_with('-'));
    }
}
