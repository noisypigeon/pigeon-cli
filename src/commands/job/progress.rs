//! The shared `indicatif` progress-bar style (CLI-0147 §0).
//!
//! Hoisted out of `email_sync::sink`, where it only ever lived because that
//! is where this codebase's first progress bar happened to be needed
//! (CLI-0013). There is nothing email-specific about it, and at two
//! consumers -- `email_import` and `deduplicate` -- a shared module is the
//! right home rather than a copy in each, the same stated exception
//! CLI-0131 §3 made for `download.rs`. The module is named for what it
//! holds, following CLI-0133 §9's `download.rs` -> `local.rs` rename.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Builds a `ProgressBar` with the style shared by every phase of every
/// job, so they read consistently in scrollback: `{prefix} {bar:40}
/// {pos}/{len} {msg}`. The trailing `{msg}` (CLI-0099) renders empty unless
/// a caller calls `set_message` -- additive and safe for every bar that
/// never sets one. Registered on `multi_progress` so concurrent workers
/// (CLI-0014) render as simultaneous lines rather than overwriting each
/// other.
///
/// The prefix is bounded to 24 characters by the `{prefix:24!}` template
/// (CLI-0033), so a long mailbox or bucket name cannot push the bar off the
/// line.
pub(crate) fn new_progress_bar(
    prefix: String,
    len: u64,
    multi_progress: &MultiProgress,
) -> ProgressBar {
    let bar = ProgressBar::new(len);
    if let Ok(style) = ProgressStyle::with_template("{prefix:24!} {bar:40} {pos}/{len} {msg}") {
        bar.set_style(style);
    }
    bar.set_prefix(prefix);
    multi_progress.add(bar)
}
