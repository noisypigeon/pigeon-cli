//! The four concrete `RcloneCommand` implementors this crate needs:
//! `Copy`/`Purge` (`Streamed`), `Lsjson`/`CopyTo` (`OneShot`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::{Labels, Observation, Outcome, RcloneCommand, RcloneLogSummary, StreamHooks};

/// `rclone copy <source> <destination>`. Struct literal with
/// `..Default::default()`, not a builder -- every field is a plain value
/// with an obvious default.
#[derive(Default)]
pub(crate) struct Copy {
    pub source: String,
    pub destination: String,
    pub log_path: PathBuf,
    pub transfers: usize,
    pub checkers: usize,
    pub tpslimit: Option<usize>,
    /// Restricts the copy to exactly the relative paths listed in this
    /// file, via `--files-from-raw` (ADR-0124) -- a literal filename, never
    /// reinterpreted as a glob pattern.
    pub files_from: Option<PathBuf>,
    pub job: &'static str,
    pub step: &'static str,
    /// Also emit `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total`
    /// -- an upload-direction copy (`rclone-copy`, `deduplicate`'s push)
    /// sets this; a download-direction one (`transform`'s pull,
    /// `deduplicate`'s pull) does not.
    pub upload_metrics: bool,
    pub hooks: StreamHooks,
}

impl RcloneCommand for Copy {
    type Output = RcloneLogSummary;

    fn subcommand(&self) -> &'static str {
        "copy"
    }

    fn args(&self) -> Vec<String> {
        let mut args = vec![self.source.clone(), self.destination.clone()];
        args.push("--transfers".to_string());
        args.push(self.transfers.to_string());
        args.push("--checkers".to_string());
        args.push(self.checkers.to_string());
        if let Some(tpslimit) = self.tpslimit {
            args.push("--tpslimit".to_string());
            args.push(tpslimit.to_string());
        }
        if let Some(files_from) = &self.files_from {
            args.push("--files-from-raw".to_string());
            args.push(files_from.display().to_string());
        }
        args.extend(
            [
                "--fast-list",
                "--buffer-size",
                "32M",
                "--multi-thread-streams",
                "4",
                "--multi-thread-cutoff",
                "256M",
            ]
            .into_iter()
            .map(String::from),
        );
        args
    }

    fn log_path(&self) -> Option<&Path> {
        Some(&self.log_path)
    }

    fn observe(&self) -> Observation {
        Observation::Streamed(self.hooks.clone())
    }

    fn labels(&self) -> Labels {
        Labels {
            job: self.job,
            step: self.step,
            success_outcome: "transferred",
            upload_metrics: self.upload_metrics,
        }
    }

    fn finish(&self, outcome: Outcome) -> Result<RcloneLogSummary, String> {
        if !outcome.success {
            return Err(format!(
                "rclone copy exited with status {} ({} file(s) transferred, {} error(s)); see {} for details{}",
                outcome
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
                outcome.log.transferred,
                outcome.log.errors,
                self.log_path.display(),
                if outcome.stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(" -- rclone stderr: {}", outcome.stderr.trim())
                },
            ));
        }
        Ok(outcome.log)
    }
}

/// `rclone purge <source>` -- a recursive, irreversible delete of everything
/// under `source`. No destination, no `--transfers`/`--tpslimit` -- no file
/// content moves, so no transfer concurrency or rate concept applies.
#[derive(Default)]
pub(crate) struct Purge {
    pub source: String,
    pub log_path: PathBuf,
    pub checkers: usize,
    pub job: &'static str,
    pub step: &'static str,
}

impl RcloneCommand for Purge {
    type Output = RcloneLogSummary;

    fn subcommand(&self) -> &'static str {
        "purge"
    }

    fn args(&self) -> Vec<String> {
        vec![
            self.source.clone(),
            "--checkers".to_string(),
            self.checkers.to_string(),
            "--fast-list".to_string(),
        ]
    }

    fn log_path(&self) -> Option<&Path> {
        Some(&self.log_path)
    }

    fn observe(&self) -> Observation {
        Observation::Streamed(StreamHooks::default())
    }

    fn labels(&self) -> Labels {
        Labels {
            job: self.job,
            step: self.step,
            success_outcome: "deleted",
            upload_metrics: false,
        }
    }

    fn finish(&self, outcome: Outcome) -> Result<RcloneLogSummary, String> {
        if !outcome.success {
            return Err(format!(
                "rclone purge exited with status {} ({} object(s) deleted, {} error(s)); see {} for details{}",
                outcome
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
                outcome.log.deletes,
                outcome.log.errors,
                self.log_path.display(),
                if outcome.stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(" -- rclone stderr: {}", outcome.stderr.trim())
                },
            ));
        }
        Ok(outcome.log)
    }
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct LsjsonEntry {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Path", default)]
    pub path: String,
    #[serde(rename = "Size", default)]
    pub size: u64,
}

/// `rclone lsjson <path>` -- a metadata-only listing, no bytes transferred.
/// `recursive` + `include_extensions` together give `transform`'s source
/// listing; neither given gives a flat destination listing. No `log_path`
/// at all: `lsjson`'s own stdout *is* the data this command returns, so it
/// carries no separate `--use-json-log` operation log the way `copy`/
/// `purge`/`copyto` do.
#[derive(Default)]
pub(crate) struct Lsjson {
    pub path: String,
    pub recursive: bool,
    /// Restricts the listing to these extensions, one `--include
    /// '*.<ext>'` per entry (rclone ORs repeated `--include` flags).
    /// A collection rather than one string because a single input kind can
    /// have several on-disk spellings -- `.jpg` and `.jpeg` (ADR-0139);
    /// when this held one `Option<String>`, a `jpeg` run over a directory
    /// of `.jpg` files silently matched nothing. Empty means no filter.
    pub include_extensions: Vec<String>,
    pub job: &'static str,
}

impl RcloneCommand for Lsjson {
    type Output = Vec<LsjsonEntry>;

    fn subcommand(&self) -> &'static str {
        "lsjson"
    }

    fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--files-only".to_string(),
            "--no-modtime".to_string(),
            "--no-mimetype".to_string(),
        ];
        if self.recursive {
            args.push("--recursive".to_string());
        }
        if !self.include_extensions.is_empty() {
            for extension in &self.include_extensions {
                args.push("--include".to_string());
                args.push(format!("*.{extension}"));
            }
            // One `--ignore-case` governs every filter rule, so it is
            // pushed once rather than per pattern.
            args.push("--ignore-case".to_string());
        }
        args.push(self.path.clone());
        args
    }

    fn log_path(&self) -> Option<&Path> {
        None
    }

    fn observe(&self) -> Observation {
        Observation::OneShot
    }

    fn labels(&self) -> Labels {
        Labels {
            job: self.job,
            step: "list",
            success_outcome: "listed",
            upload_metrics: false,
        }
    }

    /// A path that doesn't exist yet (the very first run against a brand
    /// new destination, or a source nothing has ever pulled from) is
    /// treated as empty, not an error -- confirmed against a real rclone
    /// local-backend run: a missing directory exits non-zero with
    /// `"directory not found"` in stderr. Pattern-matching subprocess
    /// stderr is inherently version-fragile (issue #63) -- reconfirm this
    /// text against whatever `rclone` version is actually deployed before
    /// relying on it in production.
    fn finish(&self, outcome: Outcome) -> Result<Vec<LsjsonEntry>, String> {
        if !outcome.success {
            if outcome
                .stderr
                .to_ascii_lowercase()
                .contains("directory not found")
            {
                return Ok(Vec::new());
            }
            return Err(format!(
                "rclone lsjson {} exited with status {}: {}",
                self.path,
                outcome
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
                outcome.stderr.trim(),
            ));
        }
        serde_json::from_slice(&outcome.stdout).map_err(|err| {
            format!(
                "failed to parse rclone lsjson output for {}: {err}",
                self.path
            )
        })
    }

    fn retry(&self) -> Option<(usize, Duration)> {
        Some((3, Duration::from_secs(2)))
    }
}

/// `rclone copyto <source> <destination>` -- rclone's documented
/// single-file-to-single-file-path primitive: no directory listing round
/// trip for one named object, unlike the bulk-tree `Copy`. Symmetric in
/// direction (both sides are plain strings, local or remote) -- `transform`'s
/// push uses it local-to-remote; `deduplicate`'s pull (ADR-0136) uses it
/// remote-to-local.
#[derive(Default)]
pub(crate) struct CopyTo {
    pub source: String,
    pub destination: String,
    pub log_path: PathBuf,
    pub job: &'static str,
    pub step: &'static str,
    /// Emits `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total` in
    /// `finish` when `true` -- the upload direction only (`transform`'s
    /// push); a download-direction use (`deduplicate`'s pull) leaves this
    /// `false`, matching `Copy`'s own `upload_metrics` convention.
    pub emit_upload_metrics: bool,
    /// The local file's size, read by the caller before construction when
    /// `emit_upload_metrics` is set -- `ADR-0124`'s GC may delete the local
    /// file immediately after a successful push, so it's unavailable
    /// afterwards. Unused otherwise.
    pub bytes: u64,
}

impl RcloneCommand for CopyTo {
    type Output = ();

    fn subcommand(&self) -> &'static str {
        "copyto"
    }

    fn args(&self) -> Vec<String> {
        vec![self.source.clone(), self.destination.clone()]
    }

    fn log_path(&self) -> Option<&Path> {
        Some(&self.log_path)
    }

    fn observe(&self) -> Observation {
        Observation::OneShot
    }

    fn labels(&self) -> Labels {
        Labels {
            job: self.job,
            step: self.step,
            success_outcome: "transferred",
            upload_metrics: false,
        }
    }

    /// Emits the shared `pigeon_upload_*` counters itself (ADR-0130) when
    /// `emit_upload_metrics` is set -- unlike `Copy`, there's no live log
    /// to poll deltas from, so this emits once, from the final outcome,
    /// same series as before this moved out of `push.rs`'s own caller-side
    /// emission.
    fn finish(&self, outcome: Outcome) -> Result<(), String> {
        use crate::observability::metrics::NO_BUCKET;

        if self.emit_upload_metrics {
            if outcome.success {
                ::metrics::counter!(
                    "pigeon_upload_bytes_total",
                    "pigeon_job" => self.job,
                    "instance" => crate::observability::instance(),
                    "destination_bucket" => NO_BUCKET,
                )
                .increment(self.bytes);
            }
            ::metrics::counter!(
                "pigeon_upload_outcomes_total",
                "pigeon_job" => self.job,
                "outcome" => if outcome.success { "uploaded" } else { "failed" },
                "instance" => crate::observability::instance(),
                "destination_bucket" => NO_BUCKET,
            )
            .increment(1);
        }

        if !outcome.success {
            return Err(format!(
                "rclone copyto {} -> {} exited with status {}; see {} for details{}",
                self.source,
                self.destination,
                outcome
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
                self.log_path.display(),
                if outcome.stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(" -- rclone stderr: {}", outcome.stderr.trim())
                },
            ));
        }
        Ok(())
    }

    fn retry(&self) -> Option<(usize, Duration)> {
        Some((3, Duration::from_secs(2)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First argv coverage for any implementor (ADR-0139): the `--include`
    /// construction was previously untested, which is how a filter that
    /// could only ever express one extension spelling went unnoticed.
    #[test]
    fn lsjson_emits_one_include_per_extension_and_a_single_ignore_case() {
        let args = (Lsjson {
            path: "source:jpg/".to_string(),
            recursive: true,
            include_extensions: vec!["jpg".to_string(), "jpeg".to_string()],
            job: "transform",
        })
        .args();

        let includes: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "--include")
            .map(|(_, pattern)| pattern)
            .collect();
        assert_eq!(includes, vec!["*.jpg", "*.jpeg"]);
        // rclone ORs repeated --include rules, and one --ignore-case
        // governs all of them.
        assert_eq!(args.iter().filter(|arg| *arg == "--ignore-case").count(), 1);
        assert!(args.contains(&"--recursive".to_string()));
        // The path is last, after every flag.
        assert_eq!(args.last().unwrap(), "source:jpg/");
    }

    #[test]
    fn lsjson_emits_no_include_or_ignore_case_when_unfiltered() {
        // The destination listing relies on this: it must see every
        // filename regardless of the run's input kind.
        let args = (Lsjson {
            path: "destination:jpg/".to_string(),
            job: "transform",
            ..Default::default()
        })
        .args();

        assert!(!args.contains(&"--include".to_string()));
        assert!(!args.contains(&"--ignore-case".to_string()));
        assert!(!args.contains(&"--recursive".to_string()));
        assert!(args.contains(&"--files-only".to_string()));
    }
}
