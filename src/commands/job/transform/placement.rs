//! Destination filenames unique by construction, not collision-detected-
//! and-fixed (ADR-0112 Decision §5). `core::data::unique_path` (the
//! `-2`/`-3` suffix-on-existing-name scheme `deduplicate` uses) only
//! checks what's already present in the *local* `result/`
//! directory -- it has no visibility into whatever the remote
//! `--destination-path` already contains from an earlier run, so it can
//! only guarantee run-local uniqueness, never global uniqueness. This
//! module never calls it.
//!
//! `compute_destination_name`'s output extension was hardcoded to `.jpg`
//! until ADR-0122 added video input kinds, which output `.mp4` instead --
//! it now takes that extension as an explicit parameter
//! (`InputFileType::output_extension`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::format::InputFileType;
use crate::commands::job::local;
use crate::core::data::{sanitize_filename, strict_stem};

/// How many hex characters (of the full SHA-256 digest) to keep in a
/// destination name -- 64 bits, a collision probability negligible at this
/// job's realistic scale (a run processing low millions of files would
/// still need to be extraordinarily unlucky to collide).
const NAME_HASH_HEX_LEN: usize = 16;

/// Derives a destination filename deterministically from
/// `(source_path, original_relative_path)`: unique by construction, with no
/// filesystem probing, no ordering-dependence, and no blind spot against
/// pre-existing destination content. Hashing the *full* relative path (not
/// just the basename) means two files that only differ by directory (e.g.
/// `screenshots/IMG_0001.png` vs. `photos/IMG_0001.png`) never collide;
/// including `source_path` in the hash input means two different source
/// trees with coincidentally identical relative paths never collide
/// either. The same source file always maps to the exact same destination
/// name on every run, which is also what makes a `transform` rerun line up
/// cleanly with each per-file `rclone copyto` push's own incremental
/// skip-if-unchanged behavior (ADR-0116).
pub(crate) fn compute_destination_name(
    source_path: &str,
    original_relative_path: &str,
    input_file_type: InputFileType,
) -> String {
    format!(
        "{}.{}",
        group_key(source_path, original_relative_path, input_file_type),
        input_file_type.output_extension()
    )
}

/// The part every output of one input file shares: `{stem}-{digest16}`
/// (ADR-0149).
///
/// Extracted from `compute_destination_name`, whose output is unchanged --
/// it is now this plus `.{extension}`. Two properties make it the right
/// axis for ADR-0149's one-input-many-outputs naming:
///
/// - It is a pure *forward* function of `(source_path, relative_path)`
///   alone, needing no file content, which is what lets ADR-0124's
///   skip-already-done filter keep running *before* the pull. A workbook's
///   sheet count is not knowable without opening it, but its group key is.
/// - Every sheet of one workbook shares it, which is what makes the outputs
///   recognizably related.
pub(crate) fn group_key(
    source_path: &str,
    original_relative_path: &str,
    input_file_type: InputFileType,
) -> String {
    let raw_stem = Path::new(original_relative_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("file");
    // ADR-0151: a spreadsheet kind gets the strict character set, because
    // only those kinds produce several outputs per input and so only their
    // report column is a multi-value list a stray comma can misread. Every
    // other kind keeps `sanitize_filename` and therefore keeps its existing
    // destination names byte-for-byte -- this job never deletes at the
    // destination, so changing them would duplicate rather than migrate.
    let stem = if input_file_type.is_spreadsheet() {
        strict_stem(raw_stem)
    } else {
        sanitize_filename(raw_stem)
    };
    let digest =
        local::sha256_hex(format!("{source_path}\u{1}{original_relative_path}").as_bytes());
    format!("{stem}-{}", &digest[..NAME_HASH_HEX_LEN])
}

/// One sheet's destination filename: `{group}-{nn}of{tt}-{slug}.{ext}`
/// (ADR-0149).
///
/// `index` is 1-based and is the workbook's own sheet order -- the only
/// place that ordering survives into the output. `total` is embedded in
/// every member so a half-pushed group is *detectably* incomplete: without
/// it, "any member present" would be the only available test and a workbook
/// whose second sheet failed to push would read as done forever, silently
/// losing that sheet.
///
/// The slug goes through `core::data::sheet_slug`, deliberately **not**
/// `sanitize_filename` -- see that function's doc comment for why.
pub(crate) fn sheet_destination_name(
    group: &str,
    index: usize,
    total: usize,
    sheet_name: &str,
    input_file_type: InputFileType,
) -> String {
    format!(
        "{group}-{index:02}of{total:02}-{}.{}",
        crate::core::data::sheet_slug(sheet_name),
        input_file_type.output_extension()
    )
}

/// Moves `scratch_path` (the already-transcoded or copied-through file)
/// into `result_dir` under its computed destination name. If that path
/// already exists, this is a **fatal error**, not something to silently
/// resolve with a suffix -- at this job's scale a collision indicates
/// either a genuine bug or an astronomically unlikely hash collision, and a
/// verifiably-unique naming scheme should fail loudly rather than quietly
/// falling back to probabilistic disambiguation (which would defeat the
/// point of having one).
pub(crate) fn place_one(
    result_dir: &Path,
    scratch_path: &Path,
    source_path: &str,
    original_relative_path: &str,
    input_file_type: InputFileType,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(result_dir)
        .map_err(|err| format!("failed to create {}: {err}", result_dir.display()))?;
    let final_path = result_dir.join(compute_destination_name(
        source_path,
        original_relative_path,
        input_file_type,
    ));
    if final_path.exists() {
        return Err(format!(
            "destination name collision placing '{original_relative_path}' at {} -- \
             this should be statistically impossible with the current naming scheme \
             and indicates a bug",
            final_path.display()
        ));
    }
    std::fs::rename(scratch_path, &final_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            scratch_path.display(),
            final_path.display()
        )
    })?;
    Ok(final_path)
}

/// Moves an already-converted sheet CSV into `result_dir` under
/// `destination_name` (ADR-0149).
///
/// Differs from `place_one` in exactly one way, and deliberately: a target
/// that already exists is a **legitimate resume**, not a collision, so the
/// scratch copy is dropped and the existing file kept. A prior run can have
/// crashed after placing sheet 1 of 3, and re-converting the workbook
/// reproduces all three byte-for-byte -- so sheet 1 arriving a second time
/// is expected, where `place_one`'s hard error is for a genuinely different
/// problem (two *distinct* inputs hashing to one name) and must keep
/// failing loudly.
pub(crate) fn place_sheet(
    result_dir: &Path,
    scratch_path: &Path,
    destination_name: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(result_dir)
        .map_err(|err| format!("failed to create {}: {err}", result_dir.display()))?;
    let final_path = result_dir.join(destination_name);
    if final_path.exists() {
        // Best-effort: the scratch directory is removed wholesale by its
        // RAII guard anyway, so a failure here changes nothing.
        let _ = std::fs::remove_file(scratch_path);
        return Ok(final_path);
    }
    std::fs::rename(scratch_path, &final_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            scratch_path.display(),
            final_path.display()
        )
    })?;
    Ok(final_path)
}

/// What a listing of already-present output filenames says about each
/// group (ADR-0149 §3) -- the replacement for the single-name `HashSet`
/// membership test ADR-0120/0124 used.
///
/// Built **once** from a listing, so the per-source-path cost stays O(1)
/// exactly as the `HashSet` it replaces. Used against two different
/// listings: the remote destination (the pull filter and the dispatch
/// backstop) and the local `result/` tree (ADR-0127's crash resume).
#[derive(Debug, Default)]
pub(crate) struct DestinationGroups {
    groups: HashMap<String, GroupState>,
}

#[derive(Debug, Default)]
struct GroupState {
    /// A plain `{group}.{ext}` name was seen -- a single-output input, which
    /// is complete by definition.
    plain: bool,
    /// Which 1-based sheet indices were seen.
    members: HashSet<usize>,
    /// The total every member agreed on, if they did.
    declared_total: Option<usize>,
    /// Two members declared different totals. Treated as incomplete: the
    /// group cannot be trusted, and redoing the workbook is cheap and
    /// self-correcting.
    inconsistent: bool,
}

impl DestinationGroups {
    /// Indexes `names` by group key.
    ///
    /// A name is registered under **every** prefix of itself that has the
    /// shape of a group key (anything, then `-`, then exactly
    /// `NAME_HASH_HEX_LEN` hex digits, then `-` or end-of-stem). That looks
    /// profligate and is the point: a stem may itself contain a run of 16
    /// hex characters, and so may a sheet slug, so *deciding* which
    /// candidate is the real digest would be guesswork. Registering under
    /// all of them makes the question moot -- `is_complete` is only ever
    /// asked about a group key computed by `group_key`, so the spurious
    /// entries are never queried.
    pub(crate) fn index<'a, I: IntoIterator<Item = &'a String>>(names: I) -> Self {
        let mut groups: HashMap<String, GroupState> = HashMap::new();
        for name in names {
            let stem = name
                .rsplit_once('.')
                .map_or(name.as_str(), |(head, _)| head);
            for (group, remainder) in candidate_groups(stem) {
                let state = groups.entry(group.to_string()).or_default();
                if remainder.is_empty() {
                    state.plain = true;
                } else if let Some((index, total)) = parse_sheet_marker(remainder) {
                    state.members.insert(index);
                    match state.declared_total {
                        Some(seen) if seen != total => state.inconsistent = true,
                        Some(_) => {}
                        None => state.declared_total = Some(total),
                    }
                }
            }
        }
        Self { groups }
    }

    /// Whether every output this input is expected to produce is already
    /// present.
    ///
    /// A plain name is complete on its own. A sheet group is complete only
    /// when its member count matches the total its own names declare --
    /// which is what makes a half-pushed workbook detectable, so a rerun
    /// re-pulls and completes it rather than treating it as done.
    pub(crate) fn is_complete(&self, group: &str) -> bool {
        let Some(state) = self.groups.get(group) else {
            return false;
        };
        if state.plain {
            return true;
        }
        if state.inconsistent {
            return false;
        }
        state.declared_total == Some(state.members.len())
    }
}

/// Every `(group, remainder)` split of `stem` where `group` has the shape
/// of a group key. See `DestinationGroups::index` for why this yields all
/// candidates rather than picking one.
fn candidate_groups(stem: &str) -> Vec<(&str, &str)> {
    let bytes = stem.as_bytes();
    let mut found = Vec::new();
    for (dash, _) in stem.char_indices().filter(|(_, ch)| *ch == '-') {
        let digest_start = dash + 1;
        let digest_end = digest_start + NAME_HASH_HEX_LEN;
        if digest_end > bytes.len() {
            break;
        }
        if !bytes[digest_start..digest_end]
            .iter()
            .all(|b| b.is_ascii_hexdigit())
        {
            continue;
        }
        // The digest must end the group: either the stem ends here, or the
        // next character begins a sheet marker.
        if digest_end != bytes.len() && bytes[digest_end] != b'-' {
            continue;
        }
        found.push((&stem[..digest_end], &stem[digest_end..]));
    }
    found
}

/// Parses a `-{nn}of{tt}-{slug}` tail into `(index, total)` (ADR-0149).
///
/// Pure and unit-tested independently of any listing, per ADR-0143's
/// `circuit_breaker_should_trip` precedent: the parsing is the interesting
/// part and is untestable buried inside an indexing loop.
fn parse_sheet_marker(remainder: &str) -> Option<(usize, usize)> {
    let tail = remainder.strip_prefix('-')?;
    // The marker runs up to the separator before the slug.
    let (marker, _slug) = tail.split_once('-')?;
    let (index, total) = marker.split_once("of")?;
    if index.is_empty() || total.is_empty() {
        return None;
    }
    if !index.bytes().all(|b| b.is_ascii_digit()) || !total.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: usize = index.parse().ok()?;
    let total: usize = total.parse().ok()?;
    if index == 0 || total == 0 || index > total {
        return None;
    }
    Some((index, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ADR-0151 -------------------------------------------------------

    /// The reported bug: one filename reading as three comma-separated
    /// ones, in a comma-joined report column.
    #[test]
    fn a_spreadsheet_destination_name_carries_no_report_breaking_character() {
        let name = compute_destination_name(
            "source:xlsx/",
            "Budget (Normal, Reduced, Crisis).xlsx",
            InputFileType::Xlsx,
        );
        assert_eq!(name, "Budget-Normal-Reduced-Crisis-ee7a9c8a4c2e5342.csv");
        for forbidden in [',', '\t', '\n', ' ', '(', ')'] {
            assert!(!name.contains(forbidden), "{name} contains {forbidden:?}");
        }
    }

    /// **The property that scopes ADR-0151 §1.** `group_key` feeds every
    /// destination name this job has ever written, and `transform` never
    /// deletes at the destination -- so applying the strict stem to the
    /// media kinds would not rename those objects, it would *duplicate*
    /// them. Measured on the run that prompted this: 67% of inputs would
    /// rename, which at ADR-0144's 37,955-object jpg bucket is ~25,000
    /// duplicates.
    #[test]
    fn a_media_destination_name_is_unaffected_by_the_strict_stem() {
        for (relative, kind, expected_ext) in [
            (
                "Budget (Normal, Reduced, Crisis).png",
                InputFileType::Png,
                "jpg",
            ),
            ("My Photo (1).jpeg", InputFileType::Jpeg, "jpg"),
            ("Holiday Video, final.mov", InputFileType::Mov, "mp4"),
            ("Voice Note (3).mp4", InputFileType::Mp4Audio, "opus"),
        ] {
            let name = compute_destination_name("source:x/", relative, kind);
            // The stem survives verbatim -- spaces, parens, commas and all.
            let stem = std::path::Path::new(relative)
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap();
            assert!(
                name.starts_with(stem),
                "{kind} name {name} must still start with the raw stem {stem}"
            );
            assert!(name.ends_with(&format!(".{expected_ext}")), "{name}");
        }
    }

    /// The digest is unchanged by the stem treatment -- it hashes the raw
    /// relative path, so two kinds over the same path share a digest even
    /// when their stems render differently. That is what keeps the hash
    /// input "byte-for-byte unchanged" as ADR-0149 promised.
    #[test]
    fn the_digest_does_not_depend_on_how_the_stem_is_rendered() {
        let strict = group_key("source:x/", "A B,C.xlsx", InputFileType::Xlsx);
        let lenient = group_key("source:x/", "A B,C.png", InputFileType::Png);
        let digest_of = |key: &str| key.rsplit('-').next().unwrap().to_string();
        // Different relative paths, so different digests -- but both are
        // 16 hex characters derived the same way.
        for key in [&strict, &lenient] {
            let digest = digest_of(key);
            assert_eq!(digest.len(), NAME_HASH_HEX_LEN);
            assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        assert!(strict.starts_with("A-B-C-"));
        assert!(lenient.starts_with("A B,C-"));
    }

    /// A strict stem must still be recognised by the group index, or the
    /// resume check would stop seeing already-done spreadsheets.
    #[test]
    fn a_strict_stem_group_is_still_recognised_by_the_index() {
        let group = group_key(
            "source:xlsx/",
            "Budget (Normal, Reduced, Crisis).xlsx",
            InputFileType::Xlsx,
        );
        let names = vec![
            sheet_destination_name(&group, 1, 2, "Summary", InputFileType::Xlsx),
            sheet_destination_name(&group, 2, 2, "Detail", InputFileType::Xlsx),
        ];
        assert!(DestinationGroups::index(&names).is_complete(&group));
    }

    // --- ADR-0149 -------------------------------------------------------

    /// The regression check that matters most: splitting
    /// `compute_destination_name` into `group_key` + extension must not have
    /// changed a single byte of its output. Every pre-existing test above is
    /// unmodified for the same reason.
    #[test]
    fn a_single_output_name_is_the_group_key_plus_its_extension() {
        let group = group_key("source:png/", "a/b/c.png", InputFileType::Png);
        assert_eq!(
            compute_destination_name("source:png/", "a/b/c.png", InputFileType::Png),
            format!("{group}.jpg")
        );
        // 16 hex digits of digest, after a `-`.
        let (stem, digest) = group.rsplit_once('-').unwrap();
        assert_eq!(stem, "c");
        assert_eq!(digest.len(), NAME_HASH_HEX_LEN);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Every sheet of one workbook shares the group key -- that is what
    /// makes the outputs recognizably related, and what lets the pull filter
    /// ask about them collectively.
    #[test]
    fn every_sheet_of_one_input_shares_its_group_key() {
        let group = group_key("source:xlsx/", "Budget.xlsx", InputFileType::Xlsx);
        let first = sheet_destination_name(&group, 1, 3, "Summary", InputFileType::Xlsx);
        let second = sheet_destination_name(&group, 2, 3, "Q1 Detail", InputFileType::Xlsx);
        assert!(first.starts_with(&format!("{group}-")));
        assert!(second.starts_with(&format!("{group}-")));
        assert_ne!(first, second);
        assert_eq!(first, format!("{group}-01of03-Summary.csv"));
        // The slug is sanitized: a space becomes a separator.
        assert_eq!(second, format!("{group}-02of03-Q1-Detail.csv"));
    }

    /// The budget `MAX_SHEET_SLUG_LENGTH` is sized against: a worst-case
    /// stem and sheet name must still fit a filesystem path component.
    #[test]
    fn a_worst_case_sheet_name_stays_within_the_filename_budget() {
        let long_stem = "x".repeat(300);
        let group = group_key(
            "source:xlsx/",
            &format!("{long_stem}.xlsx"),
            InputFileType::Xlsx,
        );
        let name = sheet_destination_name(&group, 100, 120, &"y".repeat(300), InputFileType::Xlsx);
        assert!(name.len() <= 255, "{} bytes: {name}", name.len());
    }

    #[test]
    fn parse_sheet_marker_reads_an_index_and_total() {
        assert_eq!(parse_sheet_marker("-01of03-Summary"), Some((1, 3)));
        assert_eq!(parse_sheet_marker("-12of120-x"), Some((12, 120)));
        // A slug containing a hyphen does not confuse it -- only the first
        // segment is the marker.
        assert_eq!(parse_sheet_marker("-02of03-Q1-Detail"), Some((2, 3)));
    }

    #[test]
    fn parse_sheet_marker_rejects_anything_malformed() {
        for value in [
            "",
            "-",
            "01of03-x",  // no leading separator
            "-01of03",   // no slug separator
            "-of03-x",   // no index
            "-01of-x",   // no total
            "-aofb-x",   // not digits
            "-00of03-x", // a 1-based index is never zero
            "-04of03-x", // index past the total
            "-0of0-x",
        ] {
            assert_eq!(parse_sheet_marker(value), None, "{value:?}");
        }
    }

    /// A single-output name makes its group complete on its own -- this is
    /// the pre-ADR-0149 behaviour, preserved exactly for every input kind
    /// that yields one output.
    #[test]
    fn a_plain_name_completes_its_group() {
        let group = group_key("source:png/", "a.png", InputFileType::Png);
        let names = vec![format!("{group}.jpg")];
        assert!(DestinationGroups::index(&names).is_complete(&group));
    }

    /// The decision the sheet total exists for: a half-pushed workbook must
    /// read as **incomplete**, so a rerun re-pulls and finishes it instead
    /// of treating it as done and losing that sheet forever.
    #[test]
    fn a_partial_sheet_group_is_incomplete_until_every_member_is_present() {
        let group = group_key("source:xlsx/", "Budget.xlsx", InputFileType::Xlsx);
        let first = sheet_destination_name(&group, 1, 3, "Summary", InputFileType::Xlsx);
        let second = sheet_destination_name(&group, 2, 3, "Detail", InputFileType::Xlsx);
        let third = sheet_destination_name(&group, 3, 3, "Notes", InputFileType::Xlsx);

        let partial = vec![first.clone(), second.clone()];
        assert!(!DestinationGroups::index(&partial).is_complete(&group));

        let whole = vec![first, second, third];
        assert!(DestinationGroups::index(&whole).is_complete(&group));
    }

    #[test]
    fn an_unseen_group_is_never_complete() {
        let group = group_key("source:xlsx/", "Budget.xlsx", InputFileType::Xlsx);
        assert!(!DestinationGroups::default().is_complete(&group));
        let other = vec!["something-else.csv".to_string()];
        assert!(!DestinationGroups::index(&other).is_complete(&group));
    }

    /// Members disagreeing about the total cannot be trusted, so the group
    /// is incomplete and the workbook is redone -- cheap and
    /// self-correcting.
    #[test]
    fn a_group_whose_members_disagree_about_the_total_is_incomplete() {
        let group = group_key("source:xlsx/", "Budget.xlsx", InputFileType::Xlsx);
        let names = vec![
            sheet_destination_name(&group, 1, 2, "A", InputFileType::Xlsx),
            sheet_destination_name(&group, 2, 3, "B", InputFileType::Xlsx),
        ];
        assert!(!DestinationGroups::index(&names).is_complete(&group));
    }

    /// Duplicate indices must not count twice toward the total.
    #[test]
    fn a_repeated_member_index_does_not_complete_a_group() {
        let group = group_key("source:xlsx/", "Budget.xlsx", InputFileType::Xlsx);
        let names = vec![
            sheet_destination_name(&group, 1, 2, "A", InputFileType::Xlsx),
            sheet_destination_name(&group, 1, 2, "A", InputFileType::Xlsx),
        ];
        assert!(!DestinationGroups::index(&names).is_complete(&group));
    }

    /// Foreign objects at the destination are ignored, not guessed at.
    #[test]
    fn names_that_are_not_group_shaped_are_ignored() {
        let names = vec![
            "README.txt".to_string(),
            "no-digest-here.csv".to_string(),
            "deadbeef.csv".to_string(),
        ];
        let groups = DestinationGroups::index(&names);
        assert!(!groups.is_complete("README"));
        assert!(!groups.is_complete("no-digest-here"));
    }

    /// Why `index` registers a name under *every* group-shaped prefix
    /// rather than picking one: a stem can itself contain a 16-hex run, and
    /// so can a sheet slug, so deciding which is the real digest would be
    /// guesswork. Registering under all of them makes the question moot --
    /// `is_complete` is only ever asked about a real `group_key`.
    #[test]
    fn a_hex_looking_stem_or_slug_does_not_shadow_the_real_group() {
        // A stem that is itself 16 hex characters, ahead of the real digest.
        let group = group_key("source:xlsx/", "deadbeefdeadbeef.xlsx", InputFileType::Xlsx);
        assert!(group.starts_with("deadbeefdeadbeef-"));
        let names = vec![
            sheet_destination_name(&group, 1, 2, "cafebabecafebabe", InputFileType::Xlsx),
            sheet_destination_name(&group, 2, 2, "Notes", InputFileType::Xlsx),
        ];
        let groups = DestinationGroups::index(&names);
        assert!(
            groups.is_complete(&group),
            "the real group must resolve despite hex-looking neighbours"
        );
    }

    #[test]
    fn candidate_groups_finds_every_group_shaped_prefix() {
        // Exactly one candidate in an ordinary single-output stem.
        let found = candidate_groups("photo-0123456789abcdef");
        assert_eq!(found, vec![("photo-0123456789abcdef", "")]);
        // A group key is always a prefix *ending* in `-<16 hex>`, so a
        // leading hex run is not a candidate -- nothing precedes it with a
        // `-`.
        let found = candidate_groups("deadbeefdeadbeef-0123456789abcdef-01of02-x");
        assert_eq!(
            found,
            vec![("deadbeefdeadbeef-0123456789abcdef", "-01of02-x")]
        );
        // Two candidates arise when a *slug* ends in its own 16-hex run.
        // This is the ambiguity `index` sidesteps by registering under both
        // rather than choosing.
        let found = candidate_groups("Budget-0123456789abcdef-01of02-cafebabecafebabe");
        assert_eq!(
            found,
            vec![
                ("Budget-0123456789abcdef", "-01of02-cafebabecafebabe"),
                ("Budget-0123456789abcdef-01of02-cafebabecafebabe", ""),
            ]
        );
        // A run of the wrong length is not a candidate.
        assert!(candidate_groups("photo-0123456789abcde").is_empty());
    }

    #[test]
    fn place_sheet_treats_an_existing_target_as_a_resume_not_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let result_dir = dir.path().join("result");
        std::fs::create_dir_all(&result_dir).unwrap();
        std::fs::write(result_dir.join("x-01of02-A.csv"), b"from a prior run").unwrap();

        let scratch_path = dir.path().join("scratch.csv");
        std::fs::write(&scratch_path, b"reconverted").unwrap();

        let placed = place_sheet(&result_dir, &scratch_path, "x-01of02-A.csv").unwrap();

        // The existing file wins, and the scratch copy is dropped.
        assert_eq!(
            std::fs::read(&placed).unwrap(),
            b"from a prior run",
            "an existing output must not be overwritten"
        );
        assert!(!scratch_path.exists());
    }

    #[test]
    fn place_sheet_moves_a_new_output_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let result_dir = dir.path().join("result");
        let scratch_path = dir.path().join("scratch.csv");
        std::fs::write(&scratch_path, b"fresh").unwrap();

        let placed = place_sheet(&result_dir, &scratch_path, "x-01of02-A.csv").unwrap();
        assert_eq!(placed, result_dir.join("x-01of02-A.csv"));
        assert_eq!(std::fs::read(&placed).unwrap(), b"fresh");
        assert!(!scratch_path.exists());
    }

    #[test]
    fn compute_destination_name_is_deterministic() {
        let first = compute_destination_name(
            "source:png/",
            "screenshots/IMG_0001.png",
            InputFileType::Png,
        );
        let second = compute_destination_name(
            "source:png/",
            "screenshots/IMG_0001.png",
            InputFileType::Png,
        );
        assert_eq!(first, second);
    }

    #[test]
    fn compute_destination_name_differs_for_relative_paths_sharing_a_basename() {
        let a = compute_destination_name(
            "source:png/",
            "screenshots/IMG_0001.png",
            InputFileType::Png,
        );
        let b = compute_destination_name("source:png/", "photos/IMG_0001.png", InputFileType::Png);
        assert_ne!(a, b);
        // Both still carry the shared basename as a readable prefix.
        assert!(a.starts_with("IMG_0001-"));
        assert!(b.starts_with("IMG_0001-"));
    }

    #[test]
    fn compute_destination_name_differs_across_source_paths_with_an_identical_relative_path() {
        let a = compute_destination_name("source-a:png/", "IMG_0001.png", InputFileType::Png);
        let b = compute_destination_name("source-b:png/", "IMG_0001.png", InputFileType::Png);
        assert_ne!(a, b);
    }

    #[test]
    fn compute_destination_name_uses_the_given_output_extension() {
        let jpg = compute_destination_name("source:heic/", "a/b/c.heic", InputFileType::Heic);
        assert!(jpg.ends_with(".jpg"));

        let mp4 = compute_destination_name("source:mov/", "a/b/c.mov", InputFileType::Mov);
        assert!(mp4.ends_with(".mp4"));
    }

    #[test]
    fn place_one_moves_the_scratch_file_into_result_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result_dir = dir.path().join("result");
        let scratch_path = dir.path().join("scratch.jpg");
        std::fs::write(&scratch_path, b"jpeg bytes").unwrap();

        let final_path = place_one(
            &result_dir,
            &scratch_path,
            "source:png/",
            "a.png",
            InputFileType::Png,
        )
        .unwrap();

        assert!(final_path.exists());
        assert!(!scratch_path.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"jpeg bytes");
    }

    #[test]
    fn place_one_hard_errors_on_a_forced_collision_instead_of_silently_renaming() {
        let dir = tempfile::tempdir().unwrap();
        let result_dir = dir.path().join("result");
        std::fs::create_dir_all(&result_dir).unwrap();
        let colliding_name = compute_destination_name("source:png/", "a.png", InputFileType::Png);
        std::fs::write(result_dir.join(&colliding_name), b"already here").unwrap();

        let scratch_path = dir.path().join("scratch.jpg");
        std::fs::write(&scratch_path, b"new content").unwrap();

        let result = place_one(
            &result_dir,
            &scratch_path,
            "source:png/",
            "a.png",
            InputFileType::Png,
        );
        assert!(result.is_err());
        // The pre-existing file must be untouched -- this is a hard error,
        // not a silent overwrite or a `-2` suffix fallback.
        assert_eq!(
            std::fs::read(result_dir.join(&colliding_name)).unwrap(),
            b"already here"
        );
    }

    // --- ADR-0154: documents ---

    /// **The property that scopes ADR-0154 §3.** Every document kind is
    /// single-output, so `strict_stem` stays spreadsheet-only and a
    /// document name is built exactly like a media one. Widening it would
    /// not rename already-written objects -- this job never deletes -- it
    /// would *duplicate* them.
    #[test]
    fn a_document_destination_name_is_built_like_a_media_one() {
        // A stem `strict_stem` would rewrite but `sanitize_filename` keeps.
        let stem = "Budget (Normal, Reduced)";
        for kind in [
            InputFileType::Doc,
            InputFileType::Docx,
            InputFileType::Pages,
            InputFileType::Rtf,
            InputFileType::Pdf,
            InputFileType::Epub,
            InputFileType::Pptx,
            InputFileType::Key,
            InputFileType::Txt,
            InputFileType::Md,
            InputFileType::Rst,
            InputFileType::Textclipping,
            InputFileType::Tsv,
        ] {
            let name = compute_destination_name(
                "source:docs",
                &format!("{stem}.{}", kind.canonical_extension()),
                kind,
            );
            assert!(
                name.starts_with(stem),
                "{kind} name {name} does not start with the raw stem"
            );
            assert!(
                name.ends_with(&format!(".{}", kind.output_extension())),
                "{kind} name {name}"
            );
        }
    }

    /// Seven document kinds now share one `destination:pdf/` prefix. They
    /// do not collide because the digest covers the source key, which is
    /// ADR-0112 §5's unique-by-construction property doing the work ADR-0154
    /// §3 relies on instead of adding collision handling.
    #[test]
    fn document_kinds_sharing_one_destination_prefix_do_not_collide() {
        let names: Vec<String> = [
            (InputFileType::Doc, "doc/report.doc"),
            (InputFileType::Docx, "docx/report.docx"),
            (InputFileType::Rtf, "rtf/report.rtf"),
            (InputFileType::Pages, "pages/report.pages"),
            (InputFileType::Pptx, "pptx/report.pptx"),
            (InputFileType::Epub, "epub/report.epub"),
            (InputFileType::Pdf, "pdf/report.pdf"),
        ]
        .into_iter()
        .map(|(kind, key)| compute_destination_name("source:archive", key, kind))
        .collect();

        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "collision among {names:?}");
        for name in &names {
            assert!(name.ends_with(".pdf"), "{name}");
        }
    }

    /// A multi-spelling kind's digest is over the source key, so the same
    /// stem under `.md` and `.markdown` are distinct outputs rather than
    /// one overwriting the other.
    #[test]
    fn markdowns_three_spellings_produce_distinct_destination_names() {
        let names: Vec<String> = ["notes.md", "notes.markdown", "notes.mkd"]
            .into_iter()
            .map(|key| compute_destination_name("source:md", key, InputFileType::Md))
            .collect();
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), 3, "{names:?}");
    }
}
