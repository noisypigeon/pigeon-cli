# ADR-0143: bound the damage when `transform` is pointed at the wrong input kind

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

ADR-0142 added `--input-file-type mp4-audio` with a guard that refuses an
input still carrying a video stream, specifically so that an operator who
omits the source prefix (and therefore lands on `source:mp4/`, the same
default address a plain `mp4` video run uses) cannot silently convert a
bucket of real video into audio-only files.

The guard is correct: nothing is destroyed, and nothing wrong is pushed.
But tracing what *actually* happens to each rejected file showed the guard
is **correct without being bounded** — a misdirected run degrades messily
instead of failing cleanly. Two mechanisms, each fine in isolation,
compose badly.

### 1. The local source file is retained, not deleted

`media::transcode_audio`'s `has_video_stream` check returns `Err`, and
`process_one`'s `transcode_result.map_err(...)?` returns early, so
`remove_local_copy` — reached only after a successful `push::push_one` —
never runs. That is ADR-0124's deliberate rule: *"A file that fails is
deliberately left in place on disk for diagnosis."*

For a genuinely corrupt file that rule is right. For a **wrong-kind**
failure it buys nothing: the file is intact, it is still present at
`--source` (which is where a rerun pulls it from anyway), and the error
message already states exactly what is wrong. There is nothing to diagnose
locally, and the retained bytes are pure cost.

### 2. The circuit breaker meant to stop the run is effectively unanimous

ADR-0121's breaker trips only when
`circuit_breaker_matches == CIRCUIT_BREAKER_SAMPLE_SIZE` — all 20 of the
first 20 completions being non-retryable transcode failures. A single
success disarms it *permanently*: the enclosing guard is
`circuit_breaker_samples < CIRCUIT_BREAKER_SAMPLE_SIZE`, so once `samples`
reaches 20 with `matches < 20`, the trip condition is never evaluated
again.

### What the two compose into

For the bucket that motivated ADR-0141/0142 (~480 audio-only files among
7289, so ~6.6% of an `mp4-audio` run's files would succeed), the naive
probability that the first 20 completions are *all* wrong-kind failures is
`0.934²⁰` ≈ 26%. In practice it is higher, because rejecting a video costs
one `ffprobe` call (milliseconds, no `ffmpeg`) while an audio success costs
an `ffmpeg` Opus encode plus an `rclone copyto` push — so the instant
failures dominate early completions.

But it is not guaranteed, and the failure mode is bad: at `--concurrency 1`
a single early audio-only file disarms the breaker for the entire run. When
the breaker does not trip, the run grinds through all ~6809 video files,
failing each and **retaining every one on local disk** — roughly 80 GiB for
that bucket — until free space reaches `local::check_disk_space`'s 2 GiB
floor, at which point files begin failing for a completely unrelated
reason, obscuring the real one.

So the outcome is safe but unbounded, and its diagnosis degrades the longer
it runs. Both halves are worth fixing; neither is specific to `mp4-audio`
beyond that ADR-0142 made this class of mistake newly easy to make.

## Decision

### 1. Name the two guard signatures once, and classify them

Both ADR-0141/0142 guard messages are **pigeon's own strings**, not
`ffmpeg` stderr. Matching them is therefore matching our own constants —
materially more robust than the version-fragile subprocess-stderr matching
`is_non_retryable_transcode_error` otherwise does, and worth stating
explicitly so the distinction is not lost. Today each substring is
duplicated as a bare literal across six sites in `media.rs` (two `format!`
producers, two `NON_RETRYABLE_SIGNATURES` entries, two test assertions).

Promote them to consts and reference them from the producers, the signature
list, and a new predicate, so producer and matcher cannot drift:

```rust
const NO_VIDEO_STREAM_SIGNATURE: &str = "has no video stream";
const UNEXPECTED_VIDEO_STREAM_SIGNATURE: &str = "is not an audio-only recording";

/// A failure meaning the operator pointed this run at the wrong
/// `--input-file-type` for this file -- the file itself is intact.
/// Deliberately separate from `is_non_retryable_transcode_error`, which
/// answers a different question (does retrying help?); both are true for
/// these two signatures, and they are not otherwise interchangeable.
pub(crate) fn is_wrong_input_kind_error(err: &str) -> bool
```

Both consts remain in `NON_RETRYABLE_SIGNATURES` — no behavior change
there, just one source of truth for each substring.

### 2. Delete the local source copy on a wrong-kind failure only

In `process_one`, immediately before the existing
`transcode_result.map_err(...)?`:

```rust
if let Err(err) = &transcode_result
    && media::is_wrong_input_kind_error(err)
{
    remove_local_copy(&pending_file.absolute_path);
}
```

Non-destructive: the file is still at `--source`. No scratch output exists
to clean up either, because `ffmpeg` never ran for this failure class.
Steady-state local disk for a fully-misdirected run therefore stays bounded
by the in-flight window, exactly as ADR-0124 intended for the success path.

Every other failure class keeps ADR-0124's retain-for-diagnosis behavior
**untouched**. The pre-existing
`run_transform_job_does_not_trip_the_circuit_breaker_on_a_mostly_healthy_batch`
test already asserts that a corrupt `bad.png` survives on disk, and is left
unmodified precisely because it is the guard proving this deletion did not
over-reach into other failure classes.

`remove_local_copy`'s `tracing::warn!` message loses its "after successful
push" wording, since it now has two call contexts.

### 3. The circuit breaker trips on a strong majority, not unanimity

```rust
/// At least this many of the first `CIRCUIT_BREAKER_SAMPLE_SIZE`
/// completions must be non-retryable transcode failures to trip.
/// Unanimity (ADR-0121) made the breaker disarmable by a single
/// interleaved success, so a mixed source -- the common shape of a
/// misdirected run -- could grind through every remaining file.
const CIRCUIT_BREAKER_TRIP_THRESHOLD: usize = 18;
```

The decision is extracted into a small pure function, so it is unit-testable
without depending on completion ordering (the same extract-for-testability
move ADR-0137 made with `default_bucket_index`):

```rust
fn circuit_breaker_should_trip(samples: usize, matches: usize) -> bool {
    samples == CIRCUIT_BREAKER_SAMPLE_SIZE && matches >= CIRCUIT_BREAKER_TRIP_THRESHOLD
}
```

18 of 20 is still a decisive reading, not a marginal one: the breaker
already requires each counted failure to be transcode-stage *and*
non-retryable (ADR-0130 narrowed it to exactly that), so this is 18
same-class deterministic failures, not 18 assorted errors.
`CIRCUIT_BREAKER_SAMPLE_SIZE` stays 20.

The abort message is updated: it currently says "all 20 of the first 20"
(now wrong) and attributes the cause solely to the host's `ffmpeg` build.
Since ADR-0142 there is a second, at least as likely cause — a wrong
`--input-file-type` for the source being read — and the message now names
both.

## Out of scope

- **Checkpointing failed files** so a rerun does not re-pull and re-fail
  them. `transform` decides "already done" from the destination listing, so
  a failed file (which has no destination object) is re-pulled on every
  rerun. Real, but fixing it needs the local-state mechanism ADR-0120
  deliberately rejected for this job, since it runs on ephemeral VMs where
  local disk does not survive. Filed per ADR-0031.
- **Preserving `transform-report.txt` when the breaker aborts a run.**
  `run_transform_job` returns `Err` before its per-file `outcomes` are
  used, so the wizard's `Err` arm writes the generic
  `report_upload::write_summary_report` (the error string alone) instead —
  the operator gets the breaker message but not the list of which files
  failed. Filed per ADR-0031.
- Changing `CIRCUIT_BREAKER_SAMPLE_SIZE`, or the transcode retry/backoff
  policy.
- Any source-side filter that would pre-select audio-only files: ADR-0142
  established that no file extension distinguishes them, which is the whole
  reason the guard exists.

## Consequences

- A fully-misdirected `mp4-audio` run now aborts after ~20 files instead of
  possibly grinding through thousands, and retains none of the rejected
  files locally. Local disk no longer grows with the size of the mistake.
- **A wrong-kind failure is now the one failure class whose local source
  copy is deleted.** This is a deliberate, narrow exception to ADR-0124's
  blanket retain-on-failure rule, justified by the file being intact and
  still present at the source. An operator who wants the local copy of such
  a file must re-pull it, which costs nothing they were not going to pay on
  a rerun anyway.
- **The breaker can now trip on a run that is 10% healthy.** With a
  threshold of 18/20, a source where 1 file in 10 transcodes fine will
  still abort if the first 20 completions happen to include 18 same-class
  failures. That is the intended trade: such a run is overwhelmingly a
  misdirected one, and aborting beats processing the 10% while failing the
  rest. It is a real behavior change for any input kind, not just
  `mp4-audio`.
- The abort message now names two candidate causes rather than one, so it
  no longer misdirects an operator toward auditing their `ffmpeg` build
  when the actual problem is the flag they passed.

## Amends

ADR-0121 (its unanimous trip condition becomes an 18-of-20 majority),
ADR-0124 (one narrow exception to retain-on-failure, for wrong-kind
failures only), and ADR-0142 (bounds the blast radius of the guard it
added). Each gets a short forward-pointing "Amendment" section, per this
repo's convention for amending a merged ADR without rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `media.rs`: `is_wrong_input_kind_error` matches both guard messages,
  and rejects a corrupt-file error (`"Invalid data found when processing
  input"`) and the empty string; the existing
  `is_non_retryable_transcode_error_matches_every_known_deterministic_signature`
  still passes with the messages now built from the consts.
- `worker.rs`: `circuit_breaker_should_trip` unit tests -- `(20, 20)` and
  `(20, 18)` trip; `(20, 17)`, `(19, 19)` and `(0, 0)` do not. (A companion
  test asserting the threshold's arithmetic relationship to the sample size
  was written and then dropped: both are consts, so `clippy -D warnings`
  correctly rejects it as a compile-time tautology. The relationship is
  recorded in `CIRCUIT_BREAKER_TRIP_THRESHOLD`'s doc comment instead.)
- `worker.rs`: a deterministic end-to-end `mp4-audio` run over 3 real
  generated videos (below the 20-sample threshold, so the breaker cannot
  interfere) asserts every local source copy is gone, all 3 are recorded as
  transcode-stage failures, and nothing was pushed.
- The pre-existing
  `run_transform_job_does_not_trip_the_circuit_breaker_on_a_mostly_healthy_batch`
  test stays green **unmodified** -- 1 failure in 20 is below the new
  threshold, and its `bad.png`-is-retained assertion is what proves §2 did
  not over-reach.
- Manual: an `mp4-audio` run against a prefix holding real video --
  confirm it aborts via the breaker with the new message, and that
  `source/` is not left holding the pulled videos.
