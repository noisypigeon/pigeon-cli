//! Parses rclone's `--use-json-log` output (ADR-0101, incrementally tailed
//! per ADR-0102) into live deltas plus this run's final counts. Every field
//! read is `#[serde(default)]`/`Option`, and an unparseable line is skipped
//! rather than failing the whole parse -- robust to rclone-version field
//! drift, at the cost of silently under-counting if rclone's schema changes
//! in a way this doesn't anticipate.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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
    /// rclone's own purge-mode counter -- present on every rclone JSON
    /// stats line regardless of operation, `0` for a pure copy.
    #[serde(default)]
    deletes: u64,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RcloneLogSummary {
    pub transferred: u64,
    pub errors: u64,
    pub bytes: u64,
    pub deletes: u64,
}

/// What changed since the previous `poll()` -- the caller emits metrics
/// from these deltas rather than the raw cumulative totals, so repeated
/// polling never double-counts.
#[derive(Debug, Default, Clone)]
pub(crate) struct TailDelta {
    pub transferred: u64,
    pub errors: u64,
    pub bytes: u64,
    pub deletes: u64,
    /// Consecutive identical-cause error lines collapsed since the last
    /// poll (ADR-0106), beyond the first occurrence (already emitted live
    /// by `record_error`). Independent of the numeric fields above -- a
    /// poll can carry a non-empty `collapsed_repeats` with no new
    /// transfer/error/byte delta, or vice versa.
    pub collapsed_repeats: Vec<CollapsedErrorSummary>,
}

impl TailDelta {
    pub(crate) fn is_empty(&self) -> bool {
        self.transferred == 0 && self.errors == 0 && self.bytes == 0 && self.deletes == 0
    }
}

/// An in-progress run of consecutive error lines sharing the same cause
/// (ADR-0106) -- tracked so only the first occurrence is logged live and
/// the rest are counted, not individually logged.
#[derive(Debug, Clone)]
struct ErrorStreak {
    cause: String,
    first_key: Option<String>,
    count: u64,
}

/// A finished streak of more than one consecutive identical-cause error
/// line, ready for the caller to log as a single "N more" summary (ADR-0106).
#[derive(Debug, Clone)]
pub(crate) struct CollapsedErrorSummary {
    pub cause: String,
    pub first_key: Option<String>,
    /// Repeats beyond the first, already-logged occurrence.
    pub repeated: u64,
}

/// Incrementally reads an rclone `--use-json-log` file as it grows, for live
/// mid-run metrics (ADR-0102) -- unlike a one-shot parse, this is polled
/// repeatedly while the `rclone copy` subprocess is still running. Tracks a
/// byte offset (so each poll only reads what's new) and a buffered partial
/// line (the file's tail may be mid-write, not yet newline-terminated, when
/// a poll lands) separately from the last-seen cumulative stats (so a
/// `delta` can be computed instead of re-reporting the running total).
pub(crate) struct RcloneLogTailer {
    log_path: PathBuf,
    offset: u64,
    partial_line: String,
    cumulative: RcloneLogSummary,
    current_streak: Option<ErrorStreak>,
    /// The `step` field value this tailer's own per-object `tracing::warn!`
    /// lines carry -- `"transfer"` for a copy tailer, `"delete"` for a
    /// delete tailer (ADR-0110). Shared error-handling code, so this can't
    /// stay a bare literal the way it did when only one action existed.
    step: &'static str,
}

impl RcloneLogTailer {
    pub(crate) fn new(log_path: &Path, step: &'static str) -> Self {
        Self {
            log_path: log_path.to_path_buf(),
            offset: 0,
            partial_line: String::new(),
            cumulative: RcloneLogSummary::default(),
            current_streak: None,
            step,
        }
    }

    /// Reads and processes every complete line written since the last call,
    /// re-emitting error lines as `tracing::warn!` immediately and returning
    /// the delta in cumulative `stats` since last time (zeroed if nothing
    /// new). A log file that doesn't exist yet (rclone hasn't created it)
    /// is "no new data," not an error -- the subprocess is spawned slightly
    /// before rclone opens its `--log-file`.
    pub(crate) fn poll(&mut self) -> TailDelta {
        let mut file = match fs::File::open(&self.log_path) {
            Ok(file) => file,
            Err(_) => return self.flush_only_delta(),
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return self.flush_only_delta();
        }
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            return self.flush_only_delta();
        }
        if buf.is_empty() {
            return self.flush_only_delta();
        }

        let mut chunk = std::mem::take(&mut self.partial_line);
        chunk.push_str(&buf);

        let ends_with_newline = chunk.ends_with('\n');
        let mut lines: Vec<&str> = chunk.split('\n').collect();
        // `split` on a trailing '\n' yields one trailing empty str; a
        // non-newline-terminated tail instead yields a real partial line --
        // either way, the last element isn't a complete line to process now.
        let trailing = lines.pop().unwrap_or("");
        if !ends_with_newline {
            self.partial_line = trailing.to_string();
        }

        let before = self.cumulative.clone();
        let mut collapsed_repeats = Vec::new();
        for line in lines {
            if let Some(summary) = self.process_line(line.trim()) {
                collapsed_repeats.push(summary);
            }
        }
        self.offset += buf.len() as u64;
        if let Some(summary) = self.flush_error_streak() {
            collapsed_repeats.push(summary);
        }

        TailDelta {
            transferred: self
                .cumulative
                .transferred
                .saturating_sub(before.transferred),
            errors: self.cumulative.errors.saturating_sub(before.errors),
            bytes: self.cumulative.bytes.saturating_sub(before.bytes),
            deletes: self.cumulative.deletes.saturating_sub(before.deletes),
            collapsed_repeats,
        }
    }

    /// A `TailDelta` carrying no numeric change, but still flushing (and
    /// reporting) any error streak in progress -- used by every early-return
    /// path in `poll()` (missing file, seek/read failure, no new bytes) so a
    /// pending streak is never silently dropped just because this particular
    /// poll happened to see no new log lines.
    fn flush_only_delta(&mut self) -> TailDelta {
        let mut delta = TailDelta::default();
        if let Some(summary) = self.flush_error_streak() {
            delta.collapsed_repeats.push(summary);
        }
        delta
    }

    /// Returns a `CollapsedErrorSummary` whenever this line's processing
    /// displaced a prior error streak that had repeats worth reporting --
    /// e.g. the cause changed mid-batch -- so a streak that closes out
    /// *before* the end of this poll's whole line batch (not just the one
    /// still running when the batch ends) is never silently dropped.
    fn process_line(&mut self, line: &str) -> Option<CollapsedErrorSummary> {
        if line.is_empty() {
            return None;
        }
        let Ok(parsed) = serde_json::from_str::<RcloneLogLine>(line) else {
            return None;
        };

        if let Some(stats) = &parsed.stats {
            self.cumulative.transferred = stats.transfers;
            self.cumulative.errors = stats.errors;
            self.cumulative.bytes = stats.bytes;
            self.cumulative.deletes = stats.deletes;
        }

        if parsed.level.eq_ignore_ascii_case("error") {
            return self.record_error(parsed.object, parsed.msg);
        }
        None
    }

    /// Logs the first occurrence of a new error cause immediately (same
    /// shape as before ADR-0106) and silently counts consecutive repeats of
    /// the same cause instead of logging each one individually. Returns a
    /// summary of whatever streak was just displaced, if it had any repeats
    /// worth reporting.
    fn record_error(
        &mut self,
        key: Option<String>,
        cause: String,
    ) -> Option<CollapsedErrorSummary> {
        if let Some(streak) = &mut self.current_streak
            && streak.cause == cause
        {
            streak.count += 1;
            return None;
        }
        let flushed = self.flush_error_streak();
        let action_failed_msg = if self.step == "delete" {
            "rclone object deletion failed"
        } else {
            "rclone object transfer failed"
        };
        match &key {
            Some(k) => tracing::warn!(
                key = %k,
                step = self.step,
                error = %cause,
                "{action_failed_msg}"
            ),
            None => tracing::warn!(
                step = self.step,
                error = %cause,
                "rclone reported an error"
            ),
        }
        self.current_streak = Some(ErrorStreak {
            cause,
            first_key: key,
            count: 1,
        });
        flushed
    }

    /// Ends the current error streak (if any) and, if it had repeats beyond
    /// the one already logged live, returns a summary for the caller to log
    /// (ADR-0106). A streak of exactly 1 returns `None` -- nothing to
    /// summarize beyond what `record_error` already logged.
    fn flush_error_streak(&mut self) -> Option<CollapsedErrorSummary> {
        let streak = self.current_streak.take()?;
        if streak.count <= 1 {
            return None;
        }
        Some(CollapsedErrorSummary {
            cause: streak.cause,
            first_key: streak.first_key,
            repeated: streak.count - 1,
        })
    }

    /// The last-known cumulative totals, for the final "copy complete" log
    /// line -- call after the subprocess has exited and one last `poll()`
    /// has flushed anything written between the last tick and exit.
    pub(crate) fn summary(&self) -> RcloneLogSummary {
        self.cumulative.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_log(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.jsonl");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    fn append_log(path: &Path, contents: &str) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn polling_an_empty_log_returns_a_zeroed_delta() {
        let (_dir, path) = write_log("");
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert!(delta.is_empty());
        assert_eq!(tailer.summary().transferred, 0);
    }

    #[test]
    fn polling_a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut tailer =
            RcloneLogTailer::new(&dir.path().join("not-yet-created.jsonl"), "transfer");
        let delta = tailer.poll();
        assert!(delta.is_empty());
    }

    #[test]
    fn tolerates_garbage_and_blank_lines() {
        let (_dir, path) = write_log("not json\n\n   \n{\"level\":\"info\"}\n");
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert!(delta.is_empty());
    }

    #[test]
    fn last_stats_line_wins_within_one_poll() {
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":10,\"transfers\":1,\"errors\":0}}\n",
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":100,\"transfers\":5,\"errors\":1}}\n",
        ));
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert_eq!(delta.transferred, 5);
        assert_eq!(delta.errors, 1);
        assert_eq!(delta.bytes, 100);
        assert_eq!(tailer.summary().transferred, 5);
    }

    #[test]
    fn a_deletes_bearing_stats_line_is_tracked_distinctly_from_transfers() {
        let (_dir, path) = write_log(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":0,\"transfers\":0,\"deletes\":7,\"errors\":0}}\n",
        );
        let mut tailer = RcloneLogTailer::new(&path, "delete");
        let delta = tailer.poll();
        assert_eq!(delta.deletes, 7);
        assert_eq!(delta.transferred, 0);
        assert_eq!(delta.bytes, 0);
        assert_eq!(tailer.summary().deletes, 7);
    }

    #[test]
    fn second_poll_only_reports_the_new_delta() {
        let (_dir, path) = write_log(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":10,\"transfers\":1,\"errors\":0}}\n",
        );
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let first = tailer.poll();
        assert_eq!(first.transferred, 1);

        append_log(
            &path,
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":30,\"transfers\":3,\"errors\":0}}\n",
        );
        let second = tailer.poll();
        assert_eq!(second.transferred, 2);
        assert_eq!(second.bytes, 20);
        assert_eq!(tailer.summary().transferred, 3);
    }

    #[test]
    fn a_poll_with_no_new_bytes_returns_a_zeroed_delta() {
        let (_dir, path) = write_log(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":10,\"transfers\":1,\"errors\":0}}\n",
        );
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        tailer.poll();
        let second = tailer.poll();
        assert!(second.is_empty());
    }

    #[test]
    fn a_line_split_across_two_polls_is_counted_exactly_once() {
        let (_dir, path) = write_log(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":10,\"transfers\":1,",
        );
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let first = tailer.poll();
        assert!(first.is_empty());

        append_log(&path, "\"errors\":0}}\n");
        let second = tailer.poll();
        assert_eq!(second.transferred, 1);
        assert_eq!(tailer.summary().transferred, 1);
    }

    #[test]
    fn object_keyed_error_line_does_not_fail_parsing() {
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"error\",\"msg\":\"permission denied\",\"object\":\"foo/bar.txt\"}\n",
            "{\"level\":\"info\",\"msg\":\"done\",\"stats\":{\"bytes\":1,\"transfers\":1,\"errors\":1}}\n",
        ));
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert_eq!(delta.transferred, 1);
        assert_eq!(delta.errors, 1);
    }

    #[test]
    fn non_object_error_line_does_not_fail_parsing() {
        let (_dir, path) = write_log("{\"level\":\"error\",\"msg\":\"fatal error\"}\n");
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert_eq!(delta.errors, 0);
    }

    #[test]
    fn repeated_identical_cause_errors_collapse_into_one_summary() {
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"a.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"b.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"c.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"d.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"e.txt\"}\n",
        ));
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert_eq!(delta.collapsed_repeats.len(), 1);
        let summary = &delta.collapsed_repeats[0];
        assert_eq!(summary.cause, "Too Many Requests");
        assert_eq!(summary.repeated, 4);
    }

    #[test]
    fn a_single_error_does_not_produce_a_collapsed_summary() {
        let (_dir, path) =
            write_log("{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"a.txt\"}\n");
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert!(delta.collapsed_repeats.is_empty());
    }

    #[test]
    fn interleaved_different_causes_each_get_their_own_streak() {
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"error\",\"msg\":\"cause A\",\"object\":\"a.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"cause B\",\"object\":\"b.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"cause A\",\"object\":\"c.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"cause B\",\"object\":\"d.txt\"}\n",
        ));
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert!(delta.collapsed_repeats.is_empty());
    }

    #[test]
    fn a_streak_displaced_mid_batch_by_a_cause_change_is_still_reported() {
        // A repeats 3x, then B starts, within the same poll -- the A streak
        // closes out *before* the batch ends, not at the trailing flush, so
        // this locks in that process_line's own mid-batch flush result
        // isn't silently dropped in favor of only the final streak.
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"error\",\"msg\":\"cause A\",\"object\":\"a.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"cause A\",\"object\":\"b.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"cause A\",\"object\":\"c.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"cause B\",\"object\":\"d.txt\"}\n",
        ));
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let delta = tailer.poll();
        assert_eq!(delta.collapsed_repeats.len(), 1);
        assert_eq!(delta.collapsed_repeats[0].cause, "cause A");
        assert_eq!(delta.collapsed_repeats[0].repeated, 2);
    }

    #[test]
    fn a_streak_spanning_multiple_polls_restarts_per_poll() {
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"a.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"b.txt\"}\n",
            "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"c.txt\"}\n",
        ));
        let mut tailer = RcloneLogTailer::new(&path, "transfer");
        let first = tailer.poll();
        assert_eq!(first.collapsed_repeats.len(), 1);
        assert_eq!(first.collapsed_repeats[0].repeated, 2);

        append_log(
            &path,
            concat!(
                "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"d.txt\"}\n",
                "{\"level\":\"error\",\"msg\":\"Too Many Requests\",\"object\":\"e.txt\"}\n",
            ),
        );
        let second = tailer.poll();
        assert_eq!(second.collapsed_repeats.len(), 1);
        assert_eq!(second.collapsed_repeats[0].repeated, 1);
    }
}
