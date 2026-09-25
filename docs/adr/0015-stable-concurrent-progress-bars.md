# ADR-0015: keep `pigeon email sync`'s concurrent progress bars stable

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.

## Context

A real run of `pigeon email sync` showed its progress bars failing to update in place: the same bar (e.g. `[Gmail]/All Mail fetch`) reprinted as a brand-new scrolling line on every tick (`629/15436`, `634/15436`, `685/15436`, ...) instead of overwriting itself, and this happened across bars for multiple mailboxes interleaved in one pane. This regressed after ADR-0014 (concurrent mailbox processing), which made `sync::run_async` spawn multiple mailbox workers sharing one `indicatif::MultiProgress`.

Root cause, confirmed directly in the current code (`src/email/sync.rs`, `src/email/transform.rs`) and in `indicatif` 0.18.6's own source:

`MultiProgress` redraws by tracking how many terminal lines it drew last time, moving the cursor up, and overwriting. Any plain `println!`/`eprintln!` call that happens while bars are active bypasses that tracking entirely — the write lands on the terminal without `MultiProgress` knowing a line was added, so its next redraw's cursor-up count is wrong, and every active bar starts reprinting as new lines instead of overwriting. `MultiProgress` is a single shared instance (cloned into every worker per ADR-0014), so one worker's raw print corrupts every other concurrently-running worker's bars too — exactly matching the observed screenshot, where unrelated `[Gmail]/...` fetch bars were the ones duplicating.

Three call sites in `sync_mailbox` (`src/email/sync.rs`) do this today, all reachable while other mailboxes' bars are actively drawing:
- the "up to date" status line (`println!("{mailbox_name}: up to date ...")`, in the `pending.is_empty()` early-return branch — runs before *this* mailbox's own bar exists, but other mailboxes' bars are already live);
- the "upload failed for UID ..." warning (`eprintln!`, inside the per-UID loop, with `sync_bar` and others actively drawing);
- the "verification failed for UID ..." warning (`eprintln!`, same situation).

`transform::transform_one` (`src/email/transform.rs`) has five of its own `eprintln!` warning sites (unparseable message, missing `Date` header, unreadable file, non-UID filename, missing canonical file on a dedup merge) — called synchronously from inside `sync_mailbox`'s dedup-lock-guarded block, also while bars are live. `transform.rs` has zero `indicatif` dependency today by design (ADR-0006/0007 — it's reused standalone by `--debug transform`, which draws no bars at all), so threading a `MultiProgress` into its signature would needlessly couple a pure transform module to sync's UI concerns.

`indicatif::MultiProgress` already provides exactly the right primitives for both cases: `println(msg)` — print one line above all bars, then redraw correctly — and `suspend(f)` — hide every bar, run `f`, then redraw; its own doc comment describes this as "useful for external code that writes to the standard output."

Confirmed not implicated, no changes needed: `src/email/sink.rs` has zero `println!`/`eprintln!` calls at all; `src/email/commands.rs`'s prints all run either before any sync starts or after `sync::run` has fully returned (the runtime, and every bar, is long gone by then); `run_async`'s own post-collection `eprintln!("Error: {err}")` only runs once every worker — and thus every bar — has already finished.

## Decision

### `sync_mailbox`'s own direct output moves to `MultiProgress::println`

The "up to date" status line and the two per-UID warning `eprintln!`s (upload failed, verification failed) become `let _ = multi_progress.println(...)` calls — matching the existing tolerance elsewhere in the codebase for ignoring a print call's `Result`.

### The `transform_one` call is wrapped in `MultiProgress::suspend`

`sync_mailbox`'s existing lock-guarded block (`message_index.lock()` + `attachment_index.lock()` + the synchronous `transform::transform_one(...)` call) is wrapped in `multi_progress.suspend(|| { ... })`, so any of `transform_one`'s five internal warning sites print safely without `transform.rs` gaining any `indicatif` dependency or signature change. `suspend`'s documented internal lock (blocking other workers' prints/redraws for its duration) is a non-issue here since the wrapped call is fast and fully synchronous.

### No changes to `sink.rs`, `commands.rs`, or `run_async`'s post-collection error print

Called out explicitly, with the reasoning above, so a future reader knows these were considered and found unaffected rather than overlooked.

## Consequences

- Status/warning lines print cleanly above the live bars instead of corrupting their redraw state — fixes the exact symptom observed.
- No behavior change for `--debug sink`/`--debug transform` (single-bar or no-bar paths, per ADR-0014's scoping) — this fix is entirely within the concurrent default-`sync` flow.
- Negligible performance cost: `suspend`'s internal lock is held only for one fast, synchronous `transform_one` call per message, not for any I/O-bound `.await`.

## Out of scope

- Terminal line-wrapping from long prefixes on narrow terminals — a plausible secondary contributor to bar-rendering glitches in general, but not what the observed pattern shows and not confirmed here; deferred unless it resurfaces after this fix. ([#21](https://github.com/noisypigeon/pigeon-cli/issues/21))
- Any change to `transform.rs`'s warning messages themselves or its lenient-skip behavior — only *how* (not *whether* or *what*) they print changes.
- Implementation itself — like every ADR before it, this is a decision record only.
