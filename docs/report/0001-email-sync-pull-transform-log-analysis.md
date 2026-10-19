# Report 0001: `email-sync` / `pull-transform` job-run log analysis

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-28.
- **Method**: `.claude/skills/analyze-job-run/SKILL.md` (CLI-0078), applied to
  the real `pigeon.jsonl` observability log plus three terminal transcripts.

Identities, mailbox names, and file paths below are placeholders
(`jane-doe`, `john-roe`, and similar) standing in for the real values seen
in the source log/transcripts, preserving structure and relationships
without carrying the real information.

## Correction to an earlier draft

An initial JSONL-only pass claimed "zero ERROR-level events" for the
`email-sync` run analyzed below. That was wrong -- it missed the real
failures because a separate, unrelated flood of ~208K WARN/ERROR lines from
third-party crates (`html5ever`'s "foster parenting not implemented"
parser-quirk warning, `lopdf` PDF-parsing noise) buried them and wasn't
filtered out. Re-running with the correct filter (`.fields.step` present,
or `target` scoped to `pigeon::`) surfaces the real job-level failures,
which now fully reconcile with the transcripts byte-for-byte. Flagging this
openly per the skill's own instruction not to paper over a finding that
turned out wrong.

## Finding 1: `job.email-sync` `exit_code=1` -- CONFIRMED

**Summary**: The only substantial `email-sync` run in the log
(2026-09-28T00:34:42Z - 04:18:47Z, 3h44m, 7 identities) exited with
`exit_code=1`. Two transcripts the user captured (one ending in an
OVERQUOTA-driven summary line, one ending in a manual Ctrl-C amid "Too many
simultaneous connections" errors) are both about the same underlying
problem, on the same identity.

**Root cause (confirmed, numbers reconcile exactly)**: All failures trace to
one identity, `jane-doe` -- by far the largest in the run (18 mailboxes,
176,538 pending messages, 18.2GB). IMAP-level connectivity failures against
its provider (`[OVERQUOTA] Account exceeded command or bandwidth limits`
and, in a related run, "Too many simultaneous connections") exhausted
CLI-0071's single-retry policy and caused whole batches to be dropped:

- `step=connect`, "batch retries exhausted, dropping": 2 batches x 60 UIDs
  = **120** -- matches the transcript's "120 connect" exactly.
- `step=examine`, "batch retries exhausted, dropping": 510 batches,
  summing to **115,238** UIDs -- matches "115,238 examine" exactly.
- `step=fetch` WARNs (messages whose batch was dropped upstream, never
  staged): **3,604** -- matches "3,604 missing-file" exactly.
- 120 + 115,238 + 3,604 = 118,962, matching the printed
  "118,962 failed" total exactly.

Concurrency was set to 16 for this run. That's very likely too many
simultaneous IMAP sessions for this one identity's provider to tolerate,
even with CLI-0071's per-identity cap -- the cap limits sessions but the
retry budget (one retry) isn't enough to ride out a rate-limit window once
tripped.

**A second, previously-invisible defect surfaced by this**: the merge/dedup
pass afterward tries to read every UID listed in the manifest for the
dropped mailboxes regardless of whether fetch actually staged the file,
producing hundreds of per-UID `Warning: failed to read
.../jane-doe/staging/b-sources/john-roe/{UID}.eml: No such file or
directory` lines (one per UID in the affected mailbox -- confirmed against
the transcript: exactly the 756-message range of a single source
sub-folder). **These never reach the JSONL log at all** -- confirmed by
grepping the whole file for "No such file"/"failed to read": zero matches.
They're `eprintln!`-only, invisible to this skill's entire premise that the
JSONL is the durable record. Root cause of *this* run was still solvable
because the user had the transcript, but a future crash-before-transcript
capture of this exact failure would be undiagnosable from the log alone.

**Evidence**: `jq` timestamp-bounded queries against the
00:34:42-04:18:47 window, grouped by `.fields.step`; cross-checked 1:1
against both captured transcripts' printed summary lines.

## Finding 2: `job.pull-transform` SIGKILL -- NOT the CLI-0076 pattern; a different, new defect found

**Summary**: A separate, substantial `pull-transform` run (a scratch
bucket-config in, a vault bucket-config out, encryption enabled,
concurrency 4) processed 17,678/17,678 pull-transform items and
12,941/12,941 place items successfully, then died mid-upload at
11,418/11,425, reported by `mise` as `killed by SIGKILL`.

**Root cause of the SIGKILL itself: still open, and NOT memory exhaustion.**
This is the important correction versus the CLI-0076 precedent: `mem_bytes`
in the resource-sample stream stayed flat at 400-820MB for the entire tail
of this run, right up to the last sample -- no growth, no swap-thrashing
disk-read signature. The crash-detection method still applies (samples
simply stop, no closing event), and it pinpoints the death precisely: last
resource sample **2026-09-29T03:03:30Z**, ~18 minutes after the last
logged per-item event. Since the process's own footprint was under 1GB
throughout, this SIGKILL was very likely **not self-inflicted** -- either
external memory pressure from something else running on the same machine
at the time, or an external kill unrelated to `pigeon`'s own resource use.
There isn't enough in the log to go further than that; this is genuinely
inconclusive and should be stated as such, not guessed at.

**A real, separate defect this run did surface**: `"Validation error
occurred"` -- a generic S3-client error string -- appears **64 times**
across the whole log, spanning both `job.email-sync` and
`job.pull-transform`, and at least three different targets (a scratch
bucket's downloads, a scratch bucket's uploads, and a vault bucket's
existence-check during email-sync for identity `jane-doe`). Most instances
retry and succeed; 5 in this run exhausted all 3 retries and became
permanent upload failures (the exact 5 the transcript showed, immediately
before the SIGKILL: 2 video files and 3 zip files). This is intermittent,
cross-cuts multiple bucket-configs and both job types, and was not
previously diagnosed under CLI-0076 -- it looks like a client-side
S3-request malformation (possibly interacting with CLI-0025's
deterministic-nonce encryption-before-upload path under retry) rather than
a provider-specific issue, given it isn't confined to one bucket-config or
provider.

**Evidence**: `jq` search for `job.pull-transform`-tagged lines (11,507
total, spanning far more wall-clock time than one run because the same
filter also catches many sub-100ms aborted invocations from unrelated test
runs); the 5 `step=upload` WARN lines with full file paths and error text;
the 64-count global search for "Validation error occurred"; the
resource-sample series from 02:58-03:03 UTC showing stable low memory; the
gap after 03:03:30Z with no further samples or closing event.

## Next actions (candidates, not yet decided)

1. **email-sync**: concurrency=16 is too aggressive for the `jane-doe`
   identity specifically, given its provider's rate limits. Worth a
   follow-up ADR to either lower per-identity concurrency adaptively, or
   extend CLI-0071's retry budget for `connect`/`examine` batch failures
   before dropping them.
2. **email-sync (observability gap)**: route the dedup/merge pass's
   `Warning: failed to read ... No such file or directory` lines through
   `tracing::warn!` (with `step`, `identity`, `uid`) instead of `eprintln!`,
   so a future occurrence is diagnosable from the JSONL log alone, matching
   the standard every other job phase already follows.
3. **pull-transform "Validation error occurred"**: worth its own
   investigation/ADR -- reproduce against a single bucket-config, capture
   the actual underlying S3 response (not just the generic mapped string),
   and check whether it correlates with the encrypt-before-upload retry path.
4. **pull-transform SIGKILL specifically**: genuinely open. If it recurs,
   the next occurrence should be correlated against system-wide memory
   pressure (Activity Monitor / `log show` around the death timestamp) since
   `pigeon`'s own footprint was not the cause here.
