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
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RcloneLogSummary {
    pub transferred: u64,
    pub errors: u64,
    pub bytes: u64,
}

/// What changed since the previous `poll()` -- the caller emits metrics
/// from these deltas rather than the raw cumulative totals, so repeated
/// polling never double-counts.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TailDelta {
    pub transferred: u64,
    pub errors: u64,
    pub bytes: u64,
}

impl TailDelta {
    pub(crate) fn is_empty(&self) -> bool {
        self.transferred == 0 && self.errors == 0 && self.bytes == 0
    }
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
}

impl RcloneLogTailer {
    pub(crate) fn new(log_path: &Path) -> Self {
        Self {
            log_path: log_path.to_path_buf(),
            offset: 0,
            partial_line: String::new(),
            cumulative: RcloneLogSummary::default(),
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
            Err(_) => return TailDelta::default(),
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return TailDelta::default();
        }
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            return TailDelta::default();
        }
        if buf.is_empty() {
            return TailDelta::default();
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
        for line in lines {
            self.process_line(line.trim());
        }
        self.offset += buf.len() as u64;

        TailDelta {
            transferred: self
                .cumulative
                .transferred
                .saturating_sub(before.transferred),
            errors: self.cumulative.errors.saturating_sub(before.errors),
            bytes: self.cumulative.bytes.saturating_sub(before.bytes),
        }
    }

    fn process_line(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        let Ok(parsed) = serde_json::from_str::<RcloneLogLine>(line) else {
            return;
        };

        if let Some(stats) = &parsed.stats {
            self.cumulative.transferred = stats.transfers;
            self.cumulative.errors = stats.errors;
            self.cumulative.bytes = stats.bytes;
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
        let mut tailer = RcloneLogTailer::new(&path);
        let delta = tailer.poll();
        assert!(delta.is_empty());
        assert_eq!(tailer.summary().transferred, 0);
    }

    #[test]
    fn polling_a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut tailer = RcloneLogTailer::new(&dir.path().join("not-yet-created.jsonl"));
        let delta = tailer.poll();
        assert!(delta.is_empty());
    }

    #[test]
    fn tolerates_garbage_and_blank_lines() {
        let (_dir, path) = write_log("not json\n\n   \n{\"level\":\"info\"}\n");
        let mut tailer = RcloneLogTailer::new(&path);
        let delta = tailer.poll();
        assert!(delta.is_empty());
    }

    #[test]
    fn last_stats_line_wins_within_one_poll() {
        let (_dir, path) = write_log(concat!(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":10,\"transfers\":1,\"errors\":0}}\n",
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":100,\"transfers\":5,\"errors\":1}}\n",
        ));
        let mut tailer = RcloneLogTailer::new(&path);
        let delta = tailer.poll();
        assert_eq!(delta.transferred, 5);
        assert_eq!(delta.errors, 1);
        assert_eq!(delta.bytes, 100);
        assert_eq!(tailer.summary().transferred, 5);
    }

    #[test]
    fn second_poll_only_reports_the_new_delta() {
        let (_dir, path) = write_log(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":10,\"transfers\":1,\"errors\":0}}\n",
        );
        let mut tailer = RcloneLogTailer::new(&path);
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
        let mut tailer = RcloneLogTailer::new(&path);
        tailer.poll();
        let second = tailer.poll();
        assert!(second.is_empty());
    }

    #[test]
    fn a_line_split_across_two_polls_is_counted_exactly_once() {
        let (_dir, path) = write_log(
            "{\"level\":\"info\",\"msg\":\"progress\",\"stats\":{\"bytes\":10,\"transfers\":1,",
        );
        let mut tailer = RcloneLogTailer::new(&path);
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
        let mut tailer = RcloneLogTailer::new(&path);
        let delta = tailer.poll();
        assert_eq!(delta.transferred, 1);
        assert_eq!(delta.errors, 1);
    }

    #[test]
    fn non_object_error_line_does_not_fail_parsing() {
        let (_dir, path) = write_log("{\"level\":\"error\",\"msg\":\"fatal error\"}\n");
        let mut tailer = RcloneLogTailer::new(&path);
        let delta = tailer.poll();
        assert_eq!(delta.errors, 0);
    }
}
