# ADR-0157: skip email-shaped Markdown in `transform --input-file-type md`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-17.
- **Status**: Accepted.

## Context

`source:md/` holds about 332,000 `.md` files (ADR-0156's run). A large share of
them are not notes. They are Markdown renderings of `.eml` messages, left over
from earlier email-archiving work. That email corpus is going to be redone from
scratch with `job run email import` (ADR-0146), which delivers raw `.eml` plus
attachments. These Markdown copies are therefore superseded. Converting them to
`.txt` and pushing them to `destination:txt/` is wasted `pandoc` and `copyto`
work at ~300 files/min, and it fills the destination with mail that is about to
be imported properly from the source of truth. They can stay in the source
bucket untouched.

Their keys don't give them away. The two samples that prompted this are named
`)_💪✨.md` and `)-26.md`: the subject line after sanitization, with nothing
email-specific left. ADR-0124's pre-pull filter (`files_needing_pull`) and the
dispatch choke point (`enqueue`) see keys only, so neither can tell these files
apart. Their **content** can, because it has a strong, regular shape:

~~~markdown
---
category:
tags: []
backlinks: []
status: unprocessed
sender_domain: example.com
has_attachments: false
---

# <subject>

| Field | Value |
|-------|-------|
| **Date** | 2022-01-02 02:25:41 |
| **From** | Sender <sender@example.com> |
| **To** | Recipient <recipient@example.com> |
| **CC** | _None_ |
| **Subject** | <subject> |
| **Original File** | `2022-01-02_<subject>.eml` |

## Attachments
...
## Content
<body>
---
<details>
<summary>Raw Headers</summary>

```
Delivered-To: ...
Received: ...
Return-Path: ...
DKIM-Signature: ...
Message-ID: ...
MIME-Version: 1.0
```
</details>
~~~

That isn't pigeon's own format. ADR-0006/0007's `email transform` wrote
frontmatter with `from`/`to`/`subject`/`date`, `mailbox/`/`identity/`/`sender/`
tags, `attachments:`, and `source: …eml` (later `uid:`, plus ADR-0012's
`also-in:`). The bucket may hold both generations. The detector has to catch
both without catching ordinary notes that happen to mention mail.

## Decision

### 1. Where the check runs: after the pull, before `pandoc`

Content only exists locally after the pull, so the check runs in `process_one`
(`src/commands/job/transform/worker.rs`), inside the fresh-conversion branch,
before `ScratchDir::create` and `convert_one`. It runs only when
`input_file_type == InputFileType::Md`, which covers all three spellings
(`md`/`markdown`/`mkd`, ADR-0154).

Two alternatives were rejected:

- **A key-level filter** in `files_needing_pull`/`enqueue`, like ADR-0130's
  AppleDouble and ADR-0149's lock-stub filters. That would be cheaper, since a
  skipped file would never be downloaded, but it isn't possible: the key carries
  no signal.
- **A separate pre-scan pass** reading every source object (`rclone cat` per
  key) before the pull. This is a second full-corpus traversal over 332k objects
  just to decide what the real traversal should fetch. It is the same cost
  argument ADR-0146 made against a `BODYSTRUCTURE` manifest pass, and `.md`
  files are small enough that pulling one and discarding it costs about the same
  as reading it remotely.

So the "pre-check" is per file and sits in the pipeline: each file is
classified as it arrives, just before it would be converted.

### 2. The detector: four signal families, two must agree

A new pure module, `src/commands/job/transform/email_shape.rs`, exposes
`classify(text: &str) -> Option<EmailSignals>`. `EmailSignals` records which
families matched, so the report can say why a file was skipped. The worker
reads at most the **first 64 KiB and the last 64 KiB** of the file and decodes
them as lossy UTF-8. Frontmatter and the header table sit at the top, the raw
header block sits at the bottom, and memory stays bounded no matter how large a
rendered message body is. Matching is line-based with no YAML crate, following
ADR-0012's dependency-light frontmatter handling. A frontmatter block is a
leading `---` line through the next `---` line.

| Family | Matches when |
|---|---|
| **Frontmatter headers** | frontmatter has a `sender_domain:` key, or at least two of `from:`/`to:`/`cc:`/`subject:` |
| **Provenance** | frontmatter has `has_attachments:`, `uid:`, `also-in:`, or `source:` with a value ending `.eml`, or a tag starting `mailbox/` or `identity/`; **or** the body has an `Original File` table row whose value names a `.eml` |
| **Header table** | within the first 100 lines after the frontmatter, at least three of From/To/Subject/Date appear as a `\| **X** \|` table row or a `**X:**` line |
| **Raw RFC 5322 headers** | at least three *distinct* header names from `Message-ID`, `Received`, `Return-Path`, `MIME-Version`, `DKIM-Signature`, `Delivered-To`, `ARC-Seal`, `X-Gmail-Labels` each start some line (case-insensitive) |

A file is **email-shaped if and only if at least two families match.** Header
names are matched case-insensitively, and frontmatter keys exactly as written.

Both samples match all four families. ADR-0006's format matches frontmatter
headers plus provenance (`source: …eml`, `mailbox/` tags), and ADR-0007's
matches them through `uid:`. One family alone is deliberately not enough, so
these stay transformable:

- a meeting note whose frontmatter has `from:` and `to:` (frontmatter headers only);
- a blog draft about DKIM that quotes a header block in a code fence (raw headers only);
- a travel log with a `| From | To |` table (no bold field labels, no Subject, so no family);
- a note tagged `identity/…` for an unrelated reason (provenance only).

### 3. Why the threshold leans toward transforming

Neither kind of misclassification destroys anything, because `transform` never
modifies or deletes the source (ADR-0112):

- **False negative** (an email gets through): one extra `.txt` at the
  destination. Cosmetic.
- **False positive** (a real note is skipped): the note is missing from the
  destination, but it is still at `--source`, and its key is named in the report
  (§4), so it can be found and handled by hand.

A false positive costs more to notice than a false negative costs to tolerate,
so the bar is two corroborating families, not one. Any rendered email carries
far more than one.

### 4. Accounting: a skip, not a failure

- A new `Outcome::SkippedEmailShaped` (`as_str` → `"skipped_email_shaped"`). It
  carries no `FailureStage`, and it never sets `TransformSummary.failed` or
  changes the exit code. `process_one`'s `ProcessResult` gains a way to express
  "done, nothing produced" (for example `Result<Processed, …>` with `Pushed(…)` /
  `SkippedEmailShaped(EmailSignals)`), instead of overloading an empty `Ok(vec![])`.
- A new `TransformSummary.skipped_email_shaped` tally, next to
  `skipped_apple_double` and `skipped_lock_file`. Every place those are reported
  reports it too: the `skipped:` line in `transform-report.txt`, the end-of-run
  totals `info!`, and ADR-0156's 60 s `"transform: progress"` line.
- Each skipped file still gets its own report row. `detail` names the matched
  families (e.g. `email-shaped: frontmatter-headers, provenance, header-table,
  raw-headers`), so every skip can be audited and the full list can be pulled
  with one `grep`.
- Metrics: `record_phase_count("transform", "transcode", "skipped_email_shaped",
  1, None)`. This is a new value of an existing label, so no dashboard breaks.
- **A skip does not consume a circuit-breaker sample** (ADR-0121/0143). The
  breaker estimates converter capability from early completions. A skip tells
  it nothing about that, and a bucket that is mostly email would otherwise fill
  the 20-sample window with non-evidence.
- ADR-0130's pull reconciliation is unaffected, because the key was delivered
  and dispatched as usual.

### 5. Local copy: deleted at once

A skipped file's local source copy is removed with `remove_local_copy` right
away, for the same reason ADR-0143 gives for wrong-input-kind failures: the file
is undamaged, still at `--source`, and there is nothing to diagnose. Disk use
stays bounded to the in-flight window even when most of the bucket is skipped.

### 6. Scope: `md` only, always on

- The check applies only to `--input-file-type md`. `txt`, `rst`, and other
  kinds are untouched. The detector is plain text in, signals out, so extending
  it later is just a dispatch change.
- **There is no flag** to turn it off. Every skip is auditable (§4), and a false
  positive costs nothing irreversible (§3). A wizard prompt on every `md` run,
  plus ADR-0114's `--non-interactive` fallback to reason about, isn't worth it
  for a need that has not come up. If one does, a flag can be added then.

## Consequences

- An `md` run over `source:md/` stops converting and pushing the email
  renderings. With a large share of the bucket being email, that removes a large
  share of the run's `pandoc` and `copyto` work. Each skipped file still pays
  one small pull plus a bounded read.
- The report gains one row per skipped file. That is roughly as many rows as
  those files would have produced as transcodes, so its size doesn't grow.
- `process_one`'s result type changes shape. This is internal to `worker.rs`.

## Out of scope

- **Reruns re-pull skipped files.** A skipped file leaves nothing at the ([#80](https://github.com/noisypigeon/pigeon-cli/issues/80))
  destination, so `files_needing_pull` cannot know it was already handled, and
  every rerun pulls and classifies it again. This is accepted for now because
  `.md` files are small and classifying one is cheap. It is the same "a file
  with no destination object is re-pulled forever" gap ADR-0143 deferred for
  deterministic failures, so it reuses that issue.
- **Email-shaped `.txt` files already pushed** to `destination:txt/` by
  ADR-0156's run stay there. ADR-0124's pre-pull filter treats their sources as
  already done, so this ADR never even pulls them, and `transform` never deletes
  at the destination (ADR-0112). Removing them is a separate manual step.
  Permanent: this job only ever copies.
- Deleting or moving the email-shaped sources out of `source:md/`. They are
  meant to be left behind (Context), and `transform` is read-only over its
  source. Permanent.
- Classifying any kind other than `md` (§6).

## Verification

For the implementation:

- `mise run ci` clean (fmt-check + lint + test).
- `email_shape.rs`, pure tests:
  - **Email-shaped:** both prompting samples (as trimmed fixtures with
    placeholder addresses), plus synthetic ADR-0006 (`source: …eml`) and ADR-0007
    (`uid:`) shapes. The matched-family set is asserted, not just
    `Some`.
  - **Not email-shaped:** a plain note; `from:`/`to:` frontmatter alone; a
    DKIM header block in a code fence alone; a `| From | To |` travel table; an
    `identity/` tag alone; empty input; frontmatter without a closing `---`.
  - The 64 KiB head/tail window still finds a raw-header block at the end of a
    file larger than 128 KiB.
- `worker.rs`, one `pandoc`-gated test in the style of the existing Markdown
  end-to-end test: a mixed `md` batch of one email-shaped file and one note. The
  email file must come out `Outcome::SkippedEmailShaped`, never pushed, with its
  local copy removed. The note must be transcoded. `skipped_email_shaped` must be
  `1` and `failed` must be `false`.
- Manual: rerun `job run transform --input-file-type md` over `source:md/` and
  check that `skipped_email_shaped` in `transform-report.txt` is large and
  nonzero, that a sample of skipped rows really are rendered emails, and that a
  sample of transcoded rows are not.
