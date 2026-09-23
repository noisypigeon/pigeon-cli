# ADR-0013: `pigeon email sync` progress reporting and attachment-name sanitization

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

Two real-world problems surfaced from actual `pigeon email sync` runs against Fastmail and Gmail accounts. Both are small, independent, targeted decisions discovered together — the same shape as ADR-0012 bundling attachment-dedup and whole-message-dedup into one ADR.

**Progress output goes silent mid-run.** The only `ProgressBar` in the codebase (`indicatif`, the sole progress-bar dependency) lives entirely inside `sink::fetch_uids` (`sink.rs:121-166`) and covers **only the IMAP-download phase** for a mailbox. Once it reaches `len/len` and calls `bar.finish()`, `sync::run_async`'s per-UID transform → verify → upload → delete loop (`sync.rs:131-196`) runs with **zero progress feedback** of its own. For a mailbox with thousands of messages (observed: an Archive folder at 24471 messages, an INBOX at 8070) this phase can take substantial wall-clock time while the terminal shows only the already-finished fetch bar — which reads as the program having hung. Separately, a mailbox with nothing new to fetch skips `fetch_uids` entirely via `sync.rs:120-124`'s early `continue`, producing **no output at all** for that mailbox — so on any re-run, only mailboxes with new mail print anything, and the rest silently vanish from the output.

**A sender-controlled attachment name can crash the whole run.** `transform::transform_one` (`transform.rs:211-251`) builds each attachment's on-disk path from `part.attachment_name()` — the filename the *sender's* mail client put in the MIME headers — completely un-sanitized:

```rust
let original_name = part.attachment_name().unwrap_or("attachment");
let attachment_path = unique_path(&attachments_dir.join(format!("{stem}-{original_name}")));
fs::write(&attachment_path, contents)...
```

A run against a real account reproducibly failed with:

```
Error: failed to write /.../result/willow-finch-int-gmail-com/attachments/2024-08-14-shipping-accepted-tracking-number-h0011c0006892341-/img0.png: No such file or directory (os error 2)
```

This is exactly explained by an attachment whose MIME name is `/img0.png` (leading slash): `format!("{stem}-{original_name}")` yields `...892341-/img0.png`, and `Path::join` treats the embedded `/` as a real path separator — the write target lands inside an implicit `.../892341-/` subdirectory that `fs::create_dir_all(&attachments_dir)` (`transform.rs:184`) never created (it only creates the flat `attachments_dir` itself, per ADR-0006). `fs::write` (`transform.rs:238`) then fails with `ENOENT`. Because this is a hard I/O error propagated with `?` — not one of `transform_one`'s existing lenient per-message skips (`transform.rs:124-151`, used for e.g. an unparseable message or a missing `Date` header) — it aborts the **entire** `sync` run rather than skipping just the one offending message.

## Decision

### Progress reporting covers the whole per-mailbox pipeline, not just fetch

`sync::run_async`'s per-UID loop (`sync.rs:131`) gets its own `ProgressBar`, sized to `pending.len()` and incremented once per UID regardless of outcome (synced, merged, or failed), using the same `{prefix} {bar:40} {pos}/{len}` template as the existing fetch bar so both read consistently in scrollback. The two bars are distinguished by prefix — e.g. `"{mailbox_name} fetch"` for the existing `fetch_uids` bar, `"{mailbox_name} sync"` for the new one — rather than by introducing a second visual style.

No `indicatif::MultiProgress` is needed: mailboxes, and the fetch/transform phases within a mailbox, are processed strictly sequentially (ADR-0007), so each bar cleanly finishing (a newline) before the next one starts drawing is sufficient — exactly how the single fetch bar already behaves across mailboxes today.

For a mailbox with nothing pending (`sync.rs:120-124`'s early `continue`), print a short one-line status instead of silently skipping it — e.g. `"{mailbox_name}: up to date ({n} processed)"` — so a run visibly accounts for every mailbox in the account, not just the ones with new mail. `sink::fetch_uids`'s own empty-`missing` early return (`sink.rs:127-129`, which draws no bar) needs no corresponding change: the new transform-phase bar lives independently in `sync.rs` and still runs whenever there's `pending` work, including the case where messages were already staged as `.eml` files but not yet transformed.

### Sanitize attachment names before they become part of a path

A small helper — colocated in `transform.rs` near `unique_path`, e.g. `sanitize_attachment_name` — is applied to `part.attachment_name()`'s result before it reaches `format!("{stem}-{original_name}")` (`transform.rs:237`). It strips any directory components (keeping only the final path segment) and neutralizes any remaining path-separator characters, while **preserving the file extension** — which rules out reusing `identity::sanitize_segment` for this, since that helper collapses `.` along with every other non-alphanumeric character and would corrupt extensions (`img0.png` → `img0-png`). It falls back to the existing `"attachment"` default (already used today for a missing name) if sanitization yields an empty result.

This fixes the input rather than adding a new lenient-skip branch to `transform_one`: the existing hard-I/O-failure contract (`fs::write`'s error still propagates with `?` for any *other*, genuinely unexpected I/O failure) is preserved unchanged, but a sender-controlled MIME filename can no longer trigger it.

## Consequences

- `sync` prints substantially more per-mailbox output — two bars per mailbox with new work, plus one status line per already-caught-up mailbox — but this is useful signal, not noise: the new transform/upload bar is the only feedback during what is often the longest phase of a run.
- The attachment-name fix closes a crash class: any sender-controlled MIME filename containing a path separator (or, unguarded today, `..`) could previously abort a whole `sync` run partway through; after this change such names are coerced into safe flat filenames the same way other untrusted strings (sender domain, mailbox name) already are elsewhere in this codebase.
- No frontmatter, schema, or CLI-interface changes — both fixes are internal to `sink`, `sync`, and `transform`'s existing behavior.

## Out of scope

- Adopting `indicatif::MultiProgress` for concurrent or interleaved mailbox processing — `sync` stays sequential per ADR-0007.
- Any change to `unique_path`'s existing same-name-different-content collision suffixing.
- Sanitizing other untrusted MIME-derived strings not implicated in this bug (sender display name, subject) — those already flow through the existing `sanitize_segment`/`yaml_quote` paths and aren't affected by this class of failure.
- Implementation itself — like every ADR before it, this is a decision record only.
