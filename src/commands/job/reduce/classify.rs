//! Extension-based content-value classification (ADR-0096 §2) --
//! structurally the same closed-table shape as `pull_transform::media`'s
//! `MediaKind`/format-menu pattern, just classifying for "forward or skip"
//! rather than "recode to what."

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentValue {
    Valuable,
    Reproducible,
}

/// Movie/TV video containers + software/installer/disk-image artifacts --
/// both "easily re-acquired from an external canonical source." An
/// extension absent from this table defaults to `Valuable` (the safe
/// side -- under-forwarding risks real data loss, over-forwarding only
/// costs a bit of bandwidth).
const REPRODUCIBLE_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "m4v", "ts", "mpg", "mpeg", "iso", "dmg", "exe",
    "msi", "pkg", "deb", "rpm", "appimage",
];

/// Classifies `extension` (already lowercased by `core::data::extension_of`),
/// applying the run's manual overrides first -- `force_valuable`/
/// `force_reproducible` let a user correct a misclassification without a
/// code change. Checking `force_valuable` first means a conflicting pair of
/// flags for the same extension resolves to `Valuable`, the same safe-side
/// default this function uses for an unrecognized extension.
pub(crate) fn classify_extension(
    extension: &str,
    force_valuable: &[String],
    force_reproducible: &[String],
) -> ContentValue {
    if force_valuable.iter().any(|e| e == extension) {
        return ContentValue::Valuable;
    }
    if force_reproducible.iter().any(|e| e == extension) {
        return ContentValue::Reproducible;
    }
    if REPRODUCIBLE_EXTENSIONS.contains(&extension) {
        ContentValue::Reproducible
    } else {
        ContentValue::Valuable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_video_extensions_as_reproducible() {
        assert_eq!(
            classify_extension("mp4", &[], &[]),
            ContentValue::Reproducible
        );
        assert_eq!(
            classify_extension("mkv", &[], &[]),
            ContentValue::Reproducible
        );
    }

    #[test]
    fn classifies_known_installer_extensions_as_reproducible() {
        assert_eq!(
            classify_extension("iso", &[], &[]),
            ContentValue::Reproducible
        );
        assert_eq!(
            classify_extension("dmg", &[], &[]),
            ContentValue::Reproducible
        );
    }

    #[test]
    fn defaults_unknown_extensions_to_valuable() {
        assert_eq!(classify_extension("pdf", &[], &[]), ContentValue::Valuable);
        assert_eq!(classify_extension("jpg", &[], &[]), ContentValue::Valuable);
        assert_eq!(
            classify_extension("(none)", &[], &[]),
            ContentValue::Valuable
        );
    }

    #[test]
    fn force_reproducible_overrides_the_default_valuable_classification() {
        assert_eq!(
            classify_extension("pdf", &[], &["pdf".to_string()]),
            ContentValue::Reproducible
        );
    }

    #[test]
    fn force_valuable_overrides_the_built_in_reproducible_table() {
        assert_eq!(
            classify_extension("mp4", &["mp4".to_string()], &[]),
            ContentValue::Valuable
        );
    }

    #[test]
    fn force_valuable_wins_over_force_reproducible_for_the_same_extension() {
        assert_eq!(
            classify_extension("mp4", &["mp4".to_string()], &["mp4".to_string()]),
            ContentValue::Valuable
        );
    }
}
