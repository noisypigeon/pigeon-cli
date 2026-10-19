# CLI-0075: progress visibility for `pigeon job run pull-transform`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-27.
- **Status**: Accepted. Reversed by CLI-0131.
- **Amends**: CLI-0074.

## Context

A real run against a genuinely messy bucket (17,121 pending objects
spanning `.py`/`.pyc` build artifacts, terabytes of `.mov`/`.mkv`/`.mp4`,
dozens of multi-GB `.zip` archives, hundreds of `.heic`/`.jpg`/`.pdf`, and
more) reached the final `Proceed?` confirmation, printed nothing further,
and appeared to hang indefinitely. Reading `worker.rs::run_pull_transform_job`
confirms why: **the entire concurrent download/expand/classify/recode/
verify phase has no `indicatif` progress bar and no console output of any
kind** -- unlike every other phase in this codebase (email-sync's fetch,
dedup, and upload phases all have bars per CLI-0013/0014/0015/0024; this
job's own upload phase does too, since it reuses `commands::job::upload`).
The sequential placement pass (`dedup::place_files`) is silent for the same
reason. From the user's perspective, `Proceed? yes` is the last thing
printed until the job finishes or fails, no matter how long that takes.

With a bucket this size, "how long that takes" is not short: 17,121 items
at even a modest few seconds each is easily multi-hour, and a handful of
multi-GB zips/videos can each take a real, individually-noticeable
stretch of time to download and (for video) re-encode. The fix is to give
this job the same progress-bar discipline every other job already has,
plus enough per-item detail that a long pause on one specific large/slow
file is visibly explained rather than looking identical to a genuine hang.

**Noted but explicitly out of scope here**: every object is currently
downloaded fully into memory (`Vec<u8>`) before processing (matching
`bucket::client::get_object`'s existing whole-object-in-memory design,
which every other job already relies on too). This bucket's individual
files aren't extreme (this listing's largest categories average ~1-2.5GB
per file, not tens of GB each), so it isn't the cause of the reported
hang -- but an unbounded single object remains a real, undiscovered
memory-usage ceiling in principle. Streaming large downloads to disk
would be a much bigger change touching the shared bucket client every job
depends on, and is left for its own future ADR if it ever actually bites.

## Decision

### 1. A progress bar for the main pull/process phase

`worker.rs::run_pull_transform_job` creates its `MultiProgress` up front
(previously only created later, inline, for the upload phase) and reuses
it across all three phases in this function, matching
`email_sync::worker::run_email_sync_job`'s existing one-`MultiProgress`-
spans-every-phase shape. Immediately after building the queue:
```rust
let multi_progress = MultiProgress::new();
let _ = multi_progress.println(format!("Downloading and processing {total} object(s)..."));
let bar = sink::new_progress_bar("pull-transform".to_string(), total as u64, &multi_progress);
```
(`sink::new_progress_bar`, already `pub(crate)`, is the same helper
`email_sync`'s every phase and this job's own upload phase already use.)

Each worker's loop `.inc(1)`s the bar exactly once per item that reaches a
terminal outcome (`ZipExpanded`, `Processed`, or `Failed` alike) --
alongside the existing `in_flight.fetch_sub(1, ...)` line. A zip's
expansion additionally grows the bar's total via
`bar.inc_length(members.len() as u64)` before the members are pushed onto
the queue, so newly-discovered work is reflected instead of silently
inflating the "done" percentage. `bar.finish()` once the worker pool
drains, before the placement pass's own bar begins.

This is an accepted, expected UX quirk worth calling out plainly: the
percentage can visibly *regress* right after it looked close to done, if
a late-processed zip turns out to contain a lot of members -- strictly
better than no feedback at all, and the same tradeoff any dynamic-length
progress bar makes.

### 2. Per-item status lines for the slow cases specifically

A bare percentage bar doesn't explain *why* one item is taking a long
time. Two specific, narrow announcements close that gap, both routed
through `multi_progress.println` (never a raw `println!`/`eprintln!`,
per CLI-0015's hard rule) and both deliberately rare enough not to
reproduce CLI-0071's old per-UID warning-spam problem:

- **Downloading a large object.** `download()` gains a `size: u64` and
  `multi_progress: &MultiProgress` parameter; if `size` is at or above a
  new `const ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES: u64 = 50 * 1024 * 1024`
  (50 MiB), it prints `"Downloading <key> (<size formatted>)..."` before
  the `get_object` call. This requires restoring the `size: u64` field to
  `manifest::PullTask` (removed as apparently-dead code before the "show
  which large file is in flight" need existed) and threading it through
  to `worker::QueueItem` -- populated from `ObjectEntry.size` for a
  top-level object, or `member.bytes.len() as u64` for a zip-derived one.
- **Recoding a video or audio file.** `process_media` prints
  `"Recoding <key> (<duration>s, <width>x<height>)..."` (video) or
  `"Recoding <key> (<duration>s)..."` (audio) right before calling
  `recode_and_verify`, unconditionally for `MediaKind::Video`/`Audio`
  regardless of size -- re-encoding is CPU-bound and slow independent of
  file size, unlike the cheap `mjpeg` photo/screenshot path, which prints
  nothing (it's fast enough not to need it).

`process_item`/`process_media`/`download` all gain a `multi_progress:
&MultiProgress` parameter to carry this through -- mechanical but touches
several signatures in `worker.rs`.

### 3. A progress bar for the sequential placement pass

`dedup::place_files` gains a `multi_progress: &MultiProgress` parameter
(mirroring `email_sync::dedup::run_dedup_pass`'s existing signature
exactly), creates its own `sink::new_progress_bar("place".to_string(),
files.len() as u64, multi_progress)`, `.inc(1)`s per file (whether placed,
deduped, or failed), `.finish()`s at the end. The one real call site
(`run_pull_transform_job`) passes the shared `multi_progress`; all five
existing unit tests in `dedup.rs` pass a throwaway `&MultiProgress::new()`,
the same adjustment `email_sync::dedup`'s own tests already made when its
bar was added.

## Consequences

- A `pull-transform` run now looks and behaves like every other job in
  this codebase: an immediate acknowledgment line, a live percentage bar
  that can be watched, and occasional named call-outs for the specific
  items slow enough to be worth naming.
- `download`/`process_item`/`process_media` all gain a `MultiProgress`
  parameter -- a mechanical signature change, not a behavior change,
  across several already-existing functions.
- `PullTask` regains the `size: u64` field CLI-0074's own implementation
  had just removed as unused -- a direct, visible correction, not a
  silent revert.
- The progress bar's total can grow (never shrink) as zips are expanded;
  documented as expected, not a bug.
- The in-memory-whole-object download model is unchanged and explicitly
  flagged, not fixed, here -- a future ADR's problem if a genuinely huge
  single object ever actually causes trouble.

## Out of scope

- Streaming/chunked downloads for very large single objects (would
  change `bucket::client::get_object`'s shared, whole-object-in-memory
  design, used by every job, not just this one).
- Byte-progress (as opposed to item-count progress) for the overall bar
  -- would need incremental download progress callbacks `get_object`
  doesn't currently expose.
- Any change to retry visibility (already logged to the structured
  JSONL log per CLI-0073; not surfaced to the console here, to avoid
  reintroducing per-attempt console spam).
