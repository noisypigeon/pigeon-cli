# ADR-0033: dedup-phase observability, structured failures, and three hardening fixes

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

A real user report says attachments are still not working, and asked for
this ADR to implement seven existing backlog issues (each filed via
ADR-0031's `mise run adr-issue` tooling and linked back into the ADR line
that raised it): **#42, #41, #40, #38, #37, #21, #19**.

**Before designing anything, a dedicated investigation of the current code**
(not the historical ADR-0030 description) was run to check for a fresh
attachment-loss bug. It found none: both ADR-0030 fixes -- the
`staged_attachment_path` path-reconstruction fix and the amendment's
`message_index.check`-based cross-run attachment placement fix -- are
correctly present in `dedup.rs`, exercised by passing regression tests, with
no stray `placed_md_paths` local or `staging_dir.join(staged_relpath)`
bug remaining anywhere. `transform.rs`'s phantom-zero-byte-attachment skip
(the amendment's Finding 1) is also present and covered by its own test.
`worker.rs`'s checkpoint construction and upload-task gathering don't drop
attachments either. **This ADR does not re-litigate ADR-0030** -- if
attachments are still missing, the most likely explanations are
operational (a binary not rebuilt since `a8bb348` merged, or an identity
whose already-canonicalized messages haven't been re-run since the fix
landed, which per ADR-0030 is required once to backfill orphaned
attachments) rather than a new code defect. What *is* missing is the
observability to tell the difference at a glance -- which is exactly what
five of these seven issues are about.

The seven issues cluster into three groups, each confirmed implementable
against current code by direct reads (not just the issue text):

1. **Dedup-phase visibility (#41, #40)**: `run_dedup_pass` (`dedup.rs`) is
   the one phase ADR-0032 didn't reach (it fixed the manifest phase only)
   -- it runs as a silent single pass between two bar-driven phases.
   Fixing #41 (add its bar) without #40 (fix the `eprintln!`/`suspend`
   gap) would reintroduce the exact ADR-0015 bug this ADR is also asked to
   close, this time against the *new* dedup bar -- so these two must land
   together.
2. **Failure-reason structure (#38, #37)**: `JobSummary.failed`/
   `BatchOutcome.failed` (`worker.rs:76,490-498`) is one flat `usize`
   silently merging five distinct causes -- IMAP connect failure,
   `EXAMINE` failure, any other batch-level hard error, a per-UID
   `verify_transformed` structural failure, and a per-UID lenient
   parse-skip -- and `verify_transformed` (`transform.rs:233-249`) returns
   a plain `bool`, so no reason ever leaves the function at all. This is
   *why* #38's original 29723-failure count was never conclusively
   explained: nothing captured *why* at the point of failure.
3. **Independent hardening (#42, #21, #19)**: three small, self-contained
   fixes with no cross-dependency on the other two groups or each other.

## Decision

### #41 + #40 -- dedup progress bar, suspend-safe from day one

`run_dedup_pass` (`dedup.rs:56-62`) gains a `multi_progress: &MultiProgress`
parameter. One bar, sized to twice the entry count (the existing
message-placement loop and attachment-placement loop each iterate
`entries` once -- kept structurally unchanged, per ADR-0021 §7's explicit
requirement that dedup stay a two-pass, single-threaded operation with
real ordering semantics):

```rust
let bar = sink::new_progress_bar("dedup".to_string(), (entries.len() * 2) as u64, multi_progress);
```

`bar.inc(1)` at the end of each loop's per-entry iteration, `bar.finish()`
after loop 2. The call site (`worker.rs:578`, inside `run_email_sync_job`)
already has `multi_progress` in scope -- created at `worker.rs:531`, still
live through the upload phase at `worker.rs:606` -- so threading
`&multi_progress` through is a one-line addition, no lifetime change,
matching ADR-0024 §4's identical precedent for the upload phase.

`dedup.rs:96`'s existing `eprintln!("Warning: canonical file for
duplicate...")` becomes `let _ = multi_progress.println(...)` -- now
load-bearing per ADR-0015's original rule, since this line becomes
reachable while the new dedup bar is live.

`transform.rs`'s four `eprintln!` sites (`transform.rs:91,101,109,114`)
stay in `transform.rs` unchanged, preserving that module's
zero-`indicatif`-dependency boundary (ADR-0015's original design choice).
Instead, the one call site that invokes `EmailTransform::transform`
synchronously (`worker.rs:210`, inside `process_batch_on_session`) gets
wrapped:

```rust
let transformed = multi_progress.suspend(|| transformer.transform(eml_path.clone()))?;
```

-- exactly ADR-0015's original `sync_mailbox`-wrapping pattern, now
correctly applied to the post-ADR-0021-restructure call site.
`worker.rs:551`'s task-panic `eprintln!("Error: {message}")` also converts
to `multi_progress.println`, bundled in as the same class of fix
(reachable while other workers' bars are live on the rare panic path).
`wizard.rs:577`'s top-level `fail()` is confirmed unreachable while any bar
is live (the job has already returned by then) and stays untouched.

### #37 + #38 -- structured failure-reason breakdown

`verify_transformed` (`transform.rs:233-249`) changes from returning `bool`
to `Result<(), VerifyFailure>`:

```rust
pub(crate) enum VerifyFailure {
    MissingOrEmptyMarkdown,
    MissingFrontmatterDelimiter,
    MissingOrEmptyAttachment(String), // attachment relpath
}
```

`BatchOutcome` (`worker.rs:73-77`) and `JobSummary` (`worker.rs:490-498`)
each gain a `failure_breakdown: FailureBreakdown` field:

```rust
#[derive(Default)]
pub(crate) struct FailureBreakdown {
    pub connect: usize,
    pub examine: usize,
    pub batch_error: usize,
    pub verification: usize,
    pub parse_skipped: usize,
}
```

incremented alongside the existing flat `failed` counter at each of its
five current increment sites (`worker.rs:132,147,162,238,241`) --
`failed` itself stays exactly as it is today (the sum across all five),
so nothing downstream reading a flat total breaks. The per-UID
"verification failed" warning (`worker.rs:234-237`) includes the
`VerifyFailure` detail in its text. The wizard's final summary line
(`wizard.rs:551-560`) prints the breakdown alongside the existing
sentence, e.g.:

```
Synced 370165 message(s), 29723 failed (12 connect, 40 examine, 118 batch-error, 29553 verification, 0 parse-skipped), 77897 merged, ...
```

This directly closes #37. For #38: the original 29723-count run's
per-failure detail was never captured at the time and cannot be
retroactively recovered -- this ADR closes #38 by shipping the tool that
answers the question for every run from now on (including a re-run today),
not by asserting a historical root cause the current investigation found
no evidence for either way.

### #42 -- narrow the manifest-estimate/real-count gap, and show both every run

Two changes:

1. Widen `count_attachments`'s (`manifest.rs`, added by ADR-0032) heuristic
   to also count a part carrying a `name` param on its `Content-Type`, not
   only an explicit `Content-Disposition: attachment` -- the exact
   adjustment the issue itself names as a candidate, and a real pattern for
   MUAs that never set `Content-Disposition` at all.
2. Add a ground-truth counter, `attachments_staged: usize` on `JobSummary`,
   incremented in `process_batch_on_session` (`worker.rs:207-244`) by
   `transformed.attachments.len()` per processed message. The wizard's
   final summary line prints it next to the pre-run manifest estimate
   total (already computed for `print_manifest_summary`, ADR-0032), so
   every real run makes the divergence visible by construction. The
   issue's own "measure how often/how far they diverge in practice" is
   satisfied continuously by every future run, not by a one-off
   measurement effort this ADR would otherwise have to invent from
   nothing.

### #21 -- bound progress-bar prefix width

One-line template change in the single shared helper,
`sink::new_progress_bar` (`sink.rs:136`):

```rust
// before
ProgressStyle::with_template("{prefix} {bar:40} {pos}/{len}")
// after
ProgressStyle::with_template("{prefix:24!} {bar:40} {pos}/{len}")
```

Confirmed against the pinned `indicatif 0.18.6`: the `{key:WIDTH!}`
template syntax is a static width-and-truncation spec resolved once at
`with_template()` time (not a live terminal-width query -- no new
dependency needed), and applies to any placeholder including `prefix`. A
24-char budget plus the 40-char bar and `{pos}/{len}` comfortably fits an
80-column terminal. Every bar in the codebase -- fetch, manifest, upload,
decrypt, and the new dedup bar above -- already shares this one template,
so this single line fixes every current and future bar at once.

### #19 -- properly escape control characters in frontmatter YAML values

`yaml_quote` (`core/data.rs:253-256`) currently escapes only `\` and `"`.
Sender-controlled MIME strings -- the sender display name via
`format_address` (`transform.rs:264-274`), and subject -- reach frontmatter
`from:`/`to:`/`subject:` through this one function with no other
sanitization; confirmed the only real gap, since `sender_domain_tag` and
`mailbox_tag` are already covered by `sanitize_segment`/upstream sink
sanitization respectively. The concrete risk is an embedded raw newline:
it wouldn't violate YAML's own double-quoted-scalar rules, but this
codebase's frontmatter is re-parsed as flat `\n`-split lines by
`amend_frontmatter_for_duplicate` (`core/data.rs:106-212`), so a smuggled
newline could inject a fake `tags:`/`---` line and desync dedup's
rewriter -- the same class of bug ADR-0013 fixed for attachment names,
one layer up.

Fix: extend `yaml_quote` to also escape `\n`->`\\n`, `\r`->`\\r`, and other
C0 control characters using YAML's own double-quoted escape syntax --
escaping, not stripping, since an aggressive allowlist like
`sanitize_segment` would mangle legitimate international sender names.
`yaml_quote` is called from exactly three sites, all in `transform.rs`'s
`render_frontmatter` (lines 314-316: `from`, `to`, `subject`) -- confirmed
via a full-repo grep that no other module depends on its current, narrower
escaping behavior (ADR-0020 relocated it for future reuse, but nothing
outside `email_sync` calls it yet), so strengthening it centrally fixes
all three call sites for free with no other call-site changes.

## Consequences

- The dedup phase is no longer silent, and its one warning site is now
  redraw-safe -- consistent with every other phase.
- A run's failure count is finally explainable by category instead of one
  opaque number; the wizard's summary line grows correspondingly longer.
- The `ATTACHMENTS` manifest estimate and the real staged count are shown
  side by side on every run, turning #42's "measure it" ask into a
  standing, zero-effort comparison rather than a one-time investigation.
- Progress-bar prefixes longer than 24 characters are now truncated in
  display (not in the underlying mailbox/identity name) -- a readability
  trade-off, not a data change.
- Sender display name and subject can no longer smuggle a raw newline into
  frontmatter; no change to how legitimate non-ASCII names render.
- `verify_transformed`'s signature change (`bool` -> `Result<(), _>`)
  ripples to its one caller (`worker.rs`); no other caller exists.

## Out of scope

- Retroactively determining the original 29723-run's actual root cause --
  the per-failure detail from that run no longer exists to examine; #38 is
  closed by the tooling this ADR ships, not by a historical claim.
- Any change to the dedup/placement algorithm's own logic -- ADR-0030's
  fixes are confirmed correct and untouched here.
- A live-terminal-width-aware progress bar -- indicatif's template width is
  resolved once, statically, not queried at render time; the fixed
  24-char budget is a one-time, defensible choice, not adaptive sizing. ([#46](https://github.com/noisypigeon/pigeon/issues/46))

The genuinely-deferred item above is filed as a GitHub issue via `mise run
adr-issue` while landing this ADR, per ADR-0031 §4; the first two bullets
are permanent boundaries, not backlog items, so neither gets one.
Implementation of the Decision itself is a separate, later task.
