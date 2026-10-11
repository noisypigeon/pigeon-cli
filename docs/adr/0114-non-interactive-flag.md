# ADR-0114: `--non-interactive` replaces `--yes`, suppresses every wizard prompt

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

Running `pigeon job run transform --input-file-type=png --source-path
'source:png/' --destination-path 'destination:jpg/' --report-bucket reports
--local-output /mnt/data/a --yes --concurrency 14 --transfers 16 --checkers
32` from an interactive shell hung on `rclone --tpslimit (blank = no cap):
%`, even though `--yes` was passed specifically to make the run
unattended.

Root cause: `WizardInput::resolve()` (`src/core/wizard.rs:30-39`) is the one
shared algorithm every job-wizard input goes through — `TpslimitInput`,
`SourcePathInput`, `TransfersInput`, and ~20 other structs across 7 job
wizard files all call it:

```rust
fn resolve(&self) -> Result<Self::Value, String> {
    if let Some(result) = self.flag_value() {
        return result;
    }
    if std::io::stdin().is_terminal() {
        self.prompt()
    } else {
        self.non_interactive_fallback()
    }
}
```

It decides prompt-vs-fallback purely by whether stdin is a real TTY — the
function takes no parameter and has no way to consult any CLI flag at all.
`--yes` only ever feeds one specific input, `ConfirmInput`
(`src/commands/job/shared_wizard.rs:275-300`, the final "Proceed?" gate) —
`flag_value()` there checks `self.yes` directly and short-circuits
`resolve()` before the `is_terminal()` branch is even reached. Every other
input is structurally blind to `--yes`.

This is **documented as deliberate**, not an oversight: ADR-0021 §8
("non-interactive operation is preserved") establishes that a fully
unattended run is achieved by supplying *every* input as an explicit flag
(so each one's own `flag_value()` short-circuits), with `--yes` reserved
for only the last prompt. The same framing is echoed in doc comments at
`src/commands/job/email_sync/wizard.rs:27-31,208-211` and the `EmailSync`
variant's doc comment in `src/commands/job/cli.rs:108-111`.

In practice this makes `--yes` a misleading name: the reported command ran
from an interactive shell (a real TTY), so any flag the user didn't happen
to think to pass — here `--tpslimit`, since `--transfers`/`--checkers` were
in fact supplied — fell straight through to `is_terminal()` → `true` →
blocked on `dialoguer::Input::interact_text()`. (In a genuinely
non-interactive context — cron, CI, piped/closed stdin — this exact hang
would not occur, since `is_terminal()` would already be `false` and
`non_interactive_fallback()` would fire regardless of `--yes`. The bug is
specific to "interactive shell + partial flag set + an expectation that
`--yes` means 'never prompt me'.")

Separately, note: ADR-0113 (`docs/adr/0113-deduplicate-rclone-bucket-
actions.md`, status Proposed, also dated today) is unrelated in substance
but touches the same `deduplicate/wizard.rs` file and lists `--yes` as
"unchanged" in its own flag table. That line will need a follow-up touch
if/when ADR-0113 lands after this one; it does not block this change.

## Decision

Rename `--yes` to `--non-interactive` on all 8 job CLI variants, and widen
its effect: passing it now forces **every** wizard input straight to its
`non_interactive_fallback()` — regardless of whether stdin is actually a
TTY — not only the final confirmation. This is a deliberate reversal of
ADR-0021 §8's narrow semantics: `--non-interactive` is meant to reliably
mean "never prompt me for anything," which `--yes` did not actually
deliver outside of one specific prompt.

### 1. `WizardInput::resolve()` gains a parameter — this is where the fix lives

`src/core/wizard.rs`:

```rust
fn resolve(&self, non_interactive: bool) -> Result<Self::Value, String> {
    if let Some(result) = self.flag_value() {
        return result;
    }
    if !non_interactive && std::io::stdin().is_terminal() {
        self.prompt()
    } else {
        self.non_interactive_fallback()
    }
}
```

Every individual `WizardInput` impl's `flag_value`/`prompt`/
`non_interactive_fallback` is untouched — only the trait's default
`resolve()` body changes. This creates a genuinely new interaction worth
stating explicitly: passing `--non-interactive` on a real TTY now takes
every omitted input straight to its fallback/error instead of prompting,
which was previously impossible to trigger from an interactive shell.

### 2. The flag itself: renamed and re-threaded, mechanically

- `src/commands/job/cli.rs` — `yes: bool` → `non_interactive: bool` on all
  8 job variants (`EmailSync`, `DecryptFiles`, `EmailPull`,
  `PullTransform`, `Deduplicate`, `Transform`, `RcloneAction::Copy`,
  `RcloneAction::Delete`). Clap kebab-cases the field name automatically to
  `--non-interactive`. Each variant's doc comment is rewritten to describe
  the new, uniform semantics instead of "skip the final proceed?
  confirmation" — including `EmailSync`'s explicit narrow-`--yes` comment
  and the matching ones in `email_sync/wizard.rs`.
- `src/commands/job/commands.rs` — 16 destructure/forward sites renamed
  `yes` → `non_interactive`.
- Every job's `dispatch`/`dispatch_async`/`dispatch_upload_only` signature
  (across `transform/wizard.rs`, `rclone/wizard.rs` ×2,
  `email_sync/wizard.rs` ×3, `email_pull/wizard.rs` ×3,
  `deduplicate/wizard.rs` ×3, `pull_transform/wizard.rs` ×3,
  `decrypt_files/wizard.rs` ×2): parameter renamed, and every
  `XxxInput { ... }.resolve()` call site (~40+) becomes
  `.resolve(non_interactive)`.
- `ConfirmInput` (`src/commands/job/shared_wizard.rs:275-300`): field
  renamed `pub yes: bool` → `pub non_interactive: bool`; `flag_value()`
  keeps the identical check, just against the renamed field — i.e.
  `--non-interactive` still auto-answers the final "Proceed?" as yes, same
  as `--yes` did, now for a consistent reason (it's the same flag
  suppressing the same prompt family, not a special case). Doc comment and
  the `non_interactive_fallback()` error string ("...pass --yes to skip")
  updated to match.

### 3. Tests

`tests/cli.rs`'s 36 literal `"--yes"` occurrences (help-text assertions and
actual invocations) are renamed to `"--non-interactive"`.

There is no TTY-mocking abstraction anywhere in this codebase —
`is_terminal()` is called directly with no injectable seam, and existing
unit tests already bypass `resolve()` entirely to exercise `flag_value()`/
`non_interactive_fallback()` directly. So the new "flag forces fallback
even on a real TTY" behavior is **not coverable by any existing or newly
addable automated test** in this repo as it stands today. This is called
out here explicitly as a known gap rather than silently claimed as tested;
verification for that specific behavior is manual (see Verification).

## Consequences

- **Breaking change, no migration shim** — consistent with this
  codebase's established precedent (ADR-0017, ADR-0094, ADR-0096,
  ADR-0103, ADR-0110). `--yes` disappears from the CLI; any cron job,
  deployment script, or runbook passing it must switch to
  `--non-interactive`.
- `--non-interactive` on a real TTY can now surface a *new* class of
  failure that was previously impossible from an interactive shell: a
  mandatory input with no flag and no safe default (e.g. `SourcePathInput`)
  will hard-error via its existing `non_interactive_fallback()` instead of
  prompting, even though a human is sitting at the terminal. This is the
  intended behavior, not a regression — it's the whole point of the flag —
  but worth remembering when debugging a run that errors instead of
  prompting.
- ADR-0021 §8 is superseded for all 8 job types; it remains unedited as
  historical record per this codebase's established pattern (see ADR-0021
  §Context's own note about superseding ADR-0007/0012/0014 the same way).

## Out of scope

- `read_secret()`/`confirm()` (`src/core/wizard.rs:45-82`) — free
  functions with their own independent `is_terminal()` checks, not part of
  the `WizardInput` trait, not reachable from any job wizard today (used
  elsewhere, e.g. `keyring add`'s secret entry), and no job CLI flag
  reaches them. Untouched by this ADR.
- Retroactively editing other ADRs' historical command examples that still
  show `--yes` (ADR-0023, ADR-0028, ADR-0096, ADR-0109, ADR-0110,
  ADR-0112) — these are point-in-time decision records, not live
  reference docs; only ADR-0021 gets an explicit superseded-by note here,
  since this ADR directly reverses its §8 decision.
- ADR-0113's own `--yes` table entry — left alone; that ADR is still
  Proposed/unimplemented and outside this ADR's scope.
- Adding a TTY-mocking test seam to this codebase — a real gap, but a
  larger, separate investment than this fix warrants on its own.

## Verification

- `mise run ci` (fmt-check + lint + test) clean.
- `tests/cli.rs`'s updated assertions pass (help text shows
  `--non-interactive`, not `--yes`; invocations pass `--non-interactive`).
- Manual: re-run the originally reported `transform` command with
  `--non-interactive` in place of `--yes` and `--tpslimit` still omitted —
  confirm it no longer prompts/hangs and proceeds with `tpslimit = None`.
- Manual: run any job with `--non-interactive` and a mandatory input
  omitted (no CLI flag, no sane default) from an interactive shell —
  confirm it fails fast with that input's existing
  `non_interactive_fallback()` error instead of prompting.
