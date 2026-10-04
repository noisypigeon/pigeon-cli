//! Parses rclone's `--use-json-log` output (ADR-0101) into this run's final
//! counts and per-object `tracing::warn!` events. Every field read is
//! `#[serde(default)]`/`Option`, and an unparseable line is skipped rather
//! than failing the whole parse -- robust to rclone-version field drift,
//! at the cost of silently under-counting if rclone's schema changes in a
//! way this doesn't anticipate.

use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize, Debug, Default)]
struct RcloneLogLine {
    #[serde(default)]
    level: String,
    #[serde(default)]
    msg: String,
    object: Option<String>,
    stats: Option<RcloneStats>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct RcloneStats {
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    transfers: u64,
    #[serde(default)]
    errors: u64,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RcloneLogSummary {
    pub transferred: u64,
    pub errors: u64,
    pub bytes: u64,
}

/// Reads `log_path` line by line (each line a standalone JSON object under
/// `--use-json-log`). A periodic `--stats`-interval line carries cumulative
/// totals since the run started, not a per-interval delta, so the *last*
/// stats-bearing line's totals are what's returned. Every `"level":"error"`
/// line is re-emitted as a `tracing::warn!`, matching the `key`/`step`/
/// `error` field vocabulary ADR-0073/ADR-0099 already established for
/// per-item failures elsewhere in this codebase.
pub(crate) fn parse_and_report(log_path: &Path) -> Result<RcloneLogSummary, String> {
    let contents = fs::read_to_string(log_path)
        .map_err(|err| format!("failed to read {}: {err}", log_path.display()))?;

    let mut summary = RcloneLogSummary::default();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<RcloneLogLine>(line) else {
            continue;
        };

        if let Some(stats) = &parsed.stats {
            summary.transferred = stats.transfers;
            summary.errors = stats.errors;
            summary.bytes = stats.bytes;
        }

        if parsed.level.eq_ignore_ascii_case("error") {
            match &parsed.object {
                Some(key) => tracing::warn!(
                    key = %key,
                    step = "transfer",
                    error = %parsed.msg,
                    "rclone object transfer failed"
                ),
                None => tracing::warn!(
                    step = "transfer",
                    error = %parsed.msg,
                    "rclone reported an error"
                ),
            }
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_log(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.jsonl");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn returns_zeroed_summary_for_an_empty_log() {
        let (_dir, path) = write_log("");
        let summary = parse_and_report(&path).unwrap();
        assert_eq!(summary.transferred, 0);
        assert_eq!(summary.errors, 0);
        assert_eq!(summary.bytes, 0);
    }

    #[test]
    fn tolerates_garbage_and_blank_lines() {
        let (_dir, path) = write_log("not json\n\n   \n{\"level\":\"info\"}\n");
        let summary = parse_and_report(&path).unwrap();
        assert_eq!(summary.transferred, 0);
    }

    #[test]
    fn last_stats_line_wins_over_earlier_ones() {
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":10,\"transfers\":1,\"errors\":0}}\n",
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":100,\"transfers\":5,\"errors\":1}}\n",
        ));
        let summary = parse_and_report(&path).unwrap();
        assert_eq!(summary.transferred, 5);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.bytes, 100);
    }

    #[test]
    fn object_keyed_error_line_does_not_fail_parsing() {
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"error\",\"msg\":\"permission denied\",\"object\":\"foo/bar.txt\"}\n",
            "{\"level\":\"info\",\"msg\":\"done\",\"stats\":{\"bytes\":1,\"transfers\":1,\"errors\":1}}\n",
        ));
        let summary = parse_and_report(&path).unwrap();
        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.errors, 1);
    }

    #[test]
    fn non_object_error_line_does_not_fail_parsing() {
        let (_dir, path) = write_log("{\"level\":\"error\",\"msg\":\"fatal error\"}\n");
        let summary = parse_and_report(&path).unwrap();
        assert_eq!(summary.errors, 0);
    }

    #[test]
    fn missing_log_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = parse_and_report(&dir.path().join("does-not-exist.jsonl"));
        assert!(result.is_err());
    }
}
