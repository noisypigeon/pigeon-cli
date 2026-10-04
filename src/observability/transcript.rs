//! A plain-text recording of what a single `job run` invocation printed to
//! the user (ADR-0100) -- a different artifact from `pigeon.jsonl`'s
//! structured per-event log, meant to be read by a person after the fact
//! when they weren't watching the terminal live. Deliberately not a raw
//! stdout/PTY byte capture (progress-bar redraw escape codes would make that
//! unreadable); callers record the same human-readable lines they already
//! print via `println!`.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

pub(crate) struct Transcript {
    file: Mutex<File>,
}

impl Transcript {
    /// Creates (truncating if it already exists) the transcript file at
    /// `path`.
    pub(crate) fn create(path: &Path) -> Result<Self, String> {
        let file = File::create(path)
            .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Records one line. Best-effort: an I/O failure here never aborts the
    /// job it's recording.
    pub(crate) fn line(&self, text: &str) {
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn line_appends_text_with_a_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.txt");
        let transcript = Transcript::create(&path).unwrap();
        transcript.line("hello");
        transcript.line("world");
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn create_truncates_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.txt");
        fs::write(&path, "stale content\n").unwrap();
        let transcript = Transcript::create(&path).unwrap();
        transcript.line("fresh");
        assert_eq!(fs::read_to_string(&path).unwrap(), "fresh\n");
    }
}
