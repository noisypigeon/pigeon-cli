# CLI-0164: `pigeon job run email scan`, a durable ingest ledger, and `--debug-log-bucket`/`--ledger-bucket`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-20.
- **Status**: Accepted. Implemented on 2026-10-20 (see Verification; the
  manual run against a real server is **not** done yet). Amends CLI-0100
  and CLI-0146.

## Context

The goal is a recurring email pipeline. A cheap job runs regularly from a scheduler, finds out whether an account has mail that has not been ingested, and records that, so a separate job (out of scope here) can decide to run an `email import` batch. Downstream steps, eventually including email deduplication, will rewrite what `email import` delivered.

Reading the code shows three gaps:

- **The only record of delivery is local.** `email import` records each delivered UID in `.pushed` under `--local-output/<alias>/.staging/` (`email_import/manifest.rs`), as `<mailbox>\t<uid>`, with no `UIDVALIDITY`. On a fresh or ephemeral host that file does not exist, so a scheduled import there re-fetches the whole account.
- **The destination cannot stand in for that record.** Once `deduplicate` or `transform` (or a future email dedup) processes `<sanitized-email>/<mailbox>/<uid>.eml`, those objects are renamed, merged or gone. If "what is ingested" were read from the destination, every downstream step would make the original mail look new again, and it would be copied in a second time. The thing that has to survive is a snapshot of *what was ingested*, not of *what is at the destination now*.
- **There is no way to ask "how much is new?"** without starting an import. The answer is already cheap: `gather_pending` (`email_import/mod.rs`) gets it from LIST, then `EXAMINE`, then `UIDVALIDITY`, then `UID SEARCH ALL`, with no per-message fetch at all (CLI-0146 §7).

Two flag decisions came with this. `--report-bucket` is renamed `--debug-log-bucket` on every job, because a third bucket input now sits beside it and "report" no longer says which bucket is the diagnostic one. And `--ledger-bucket` is accepted by every job, so a scheduler can pass one uniform flag set.

## Decision

### 1. `pigeon job run email scan`

`EmailAction::Scan` in `src/commands/job/cli.rs`, Observable name `job.email-scan`, implemented in `src/commands/job/email_scan/`:

| Flag | |
|---|---|
| `--identities` | as `email import`; required non-interactively |
| `--ledger-bucket` | where the ledger and status live; mandatory |
| `--destination` | read only to seed a ledger that does not exist yet (§4) |
| `--local-output` | bookkeeping, report, transcript |
| `--debug-log-bucket` | CLI-0100's artifact upload, renamed (§2) |
| `--non-interactive` | CLI-0114 |

**Deliberately absent**, and pinned by a negative CLI test: `--messages-per-minute`, `--batch-size`, `--max-connections`, `--push-concurrency` and `--legacy`. A scan transfers no message body and holds one connection per identity, so it has no pace or transfer to tune.

Per identity, a scan:
1. Connects (CLI-0146's `connect_with_retry`, same budget).
2. Lists every selectable mailbox and runs one `EXAMINE` and one `UID SEARCH ALL` on each (`email_import::list_mailbox_states`, factored out of `gather_pending` so both jobs share it).
3. Logs out.
4. Loads the ledger, or seeds it (§4).
5. Diffs each mailbox (§5), counts attachments on the new UIDs only (§5), and writes `scan-status.tsv` (§6).

It logs out before step 4 because a seed listing can take minutes, longer than a server should be asked to hold an idle session. A second connection is opened only if there is something to count.

**Scan never appends to the ledger.** Only a confirmed delivery by `email import` adds a row (§3). Seeding is the one exception, and it only happens when no ledger exists.

**One identity failing does not abandon the others.** A scheduled scan should report every account it could reach. The run exits 1 if any identity failed. Finding new mail is **not** a failure and does not change the exit code, because how a trigger is signalled is out of scope.

**No "proceed?" confirmation.** Every other job asks for one, but a scan changes nothing on the server, and its only writes are to `--ledger-bucket`.

### 2. `--debug-log-bucket` and `--ledger-bucket` on every job (amends CLI-0100)

- **`--report-bucket` is renamed `--debug-log-bucket`** on all six jobs. It is a breaking rename with **no alias** for the old spelling, following CLI-0017's no-shim precedent: the old spelling now fails as an unknown argument, which is louder than a silent alias that a cron line keeps relying on forever. `ReportBucketInput` becomes `DebugLogBucketInput`. The upload itself, the `{date}-{job}-{run-id}/` layout and `report_upload.rs` (the module name is kept, since a report is still what it uploads) do not change. `gh search code "report-bucket" --owner noisypigeon` found no external caller when this landed.
- **`--ledger-bucket` is accepted by every job.** `email scan` requires it, and so does `email import` unless `--legacy`. Every other job, and `email import --legacy`, accepts the flag and ignores it, logging one `debug!` line via `shared_wizard::note_unused_ledger_bucket` and never validating the alias. That keeps the scheduler contract simple: the same flag set on every job, from day one.
- **`LedgerBucketInput`** (`shared_wizard.rs`) resolves `alias[:prefix/]` through `RemoteTarget::parse` (CLI-0138), the same form as every other rclone-addressed target, so one bucket can hold both state and debug logs under different prefixes (`state:ledger/`, `state:logs/`). It belongs in `shared_wizard` with two consumers because it is a cross-job auxiliary bucket input like `DebugLogBucketInput`, not a job-specific prompt.
- **`Commands::Job` is boxed** (`src/cli.rs`). Every job's flags live inline in that variant, and adding a field to all of them pushed it past `clippy::large_enum_variant`.

### 3. The ingest ledger is a snapshot of ingestion, not a mirror of the destination

`src/commands/job/email_import/ledger.rs`, shared by both jobs (scan reaches it through `email_import`'s module path, as CLI-0146 §2 reached `sink`):

```
<ledger-bucket>/<sanitized-email>/ingested.tsv      # mailbox \t uidvalidity \t uid \t ingested_at
<ledger-bucket>/<sanitized-email>/scan-status.tsv   # §6
```

- **The key is `(mailbox, UIDVALIDITY, UID)`**, the same identity `email import` already resumes on, now carrying the generation. `ingested_at` is the earliest time a row was recorded (`Ledger::insert` keeps the earlier one), as `YYYY-MM-DDTHH:MM:SSZ`. It is hand-formatted, because `time`'s `formatting` feature was pruned as unused (CLI-0148), and lexicographic order is chronological order.
- **Rows are only ever added.** A server-side delete does not remove one. A `UIDVALIDITY` reset does not remove one either: the old generation's rows simply stop matching, and that mailbox reads as entirely new, which is correct, since every UID was renumbered. A downstream job rewriting or deduplicating the delivered objects also never removes a row. This is the property the whole ADR exists for: a future email dedup cannot cause the originals to be re-ingested.
- **State moves with `CopyTo` in both directions**, with `emit_upload_metrics: false`. State about a run must not inflate `pigeon_upload_bytes_total` (CLI-0148's reasoning for the debug-log artifacts).
- **Missing and unreachable are told apart by listing, not by error text.** `download_state` first runs a non-recursive `Lsjson { missing_ok: true }` over the identity's directory. An absent object is `Ok(None)`. Any other failure, including an unreachable or misconfigured ledger bucket, is an `Err` that fails the identity. Reading an unreachable ledger as empty would re-ingest an entire account. Matching rclone's stderr is the fragility issue #63 records, and CLI-0140's `missing_ok` already draws exactly this line.
- **Parsing is lenient.** `#` lines, blank lines and malformed rows are skipped, as the `.pushed` loader does. A torn final line from an interrupted write is therefore dropped, and its message reads as not yet ingested. That is the safe direction: an extra copy at the destination, never a lost one.

### 4. Seeding, the first time an identity is seen

When no ledger exists, `ledger::load_or_seed` builds one from what `email import` has already delivered:
1. A recursive `Lsjson` (`include_extensions: ["eml"]`, `missing_ok: true`) over `<destination>/<sanitized-email>/`. A never-imported account has no directory, which is a valid empty seed.
2. Every `<mailbox-relpath>/<uid>.eml` key is mapped back to its raw mailbox name by sanitizing the server's current mailbox list with `sink::sanitize_mailbox_path`, the same function that produced the key (`seed_from_listing`, pure).
3. The result is uploaded immediately, so a concurrent scan or import sees it.

Attachment keys (`<relpath>/<uid>/attachments/...`) and anything else that is not `<relpath>/<digits>.eml` are ignored. Both jobs can seed. Scan needs `--destination` only for this. If scan has no `--destination` when it needs one, it fails with `no ledger for '<alias>' at <address>; --destination is required to seed it`, and it never prompts for it.

Recorded limits, each stated rather than hidden:
- **Every seeded row gets the mailbox's current `UIDVALIDITY`**, the only one knowable from a listing. If it changed between the original import and the seed, the seeded rows name the wrong messages, and nothing here can detect that.
- **A relpath that two server mailboxes sanitize to is ambiguous.** Its rows are skipped with a WARN naming it, so those messages read as new rather than being marked ingested under the wrong mailbox.
- **A destination already processed downstream under-seeds.** Messages no longer present as `<uid>.eml` read as new, and the next import ingests them again. Seeding before any downstream job runs avoids this, and an extra copy is the recoverable direction.

### 5. What a scan computes

For each mailbox, against the ledger rows with the same `(mailbox, uidvalidity)` (`status::diff`, pure):

- `new_messages`: on the server, not in the ledger.
- `ingested`: ledger rows for this generation.
- `removed_on_server`: in the ledger, no longer on the server. Informational only; the ledger keeps them.
- `server_messages = ingested - removed_on_server + new_messages` always holds, and a unit test pins it.

**New attachments are counted from `BODYSTRUCTURE` over the new UIDs only** (`email_scan/bodystructure.rs`): `UID FETCH <set> (UID BODYSTRUCTURE)` in batches of 50, MIME structure metadata and never a body.
- The counting heuristic is CLI-0032/0033's: a leaf with an `attachment` disposition or a `name` content-type parameter. It was removed with `email-sync` in CLI-0147 (`4b066e9`) and is brought back trimmed to its counting half.
- It includes CLI-0065's bisection. One unparseable `BODYSTRUCTURE` (`MESSAGE/DELIVERY-STATUS`, `MESSAGE/GLOBAL`) fails the whole batched response in imap-proto, so a failed batch is split until the poisoned UID is isolated. That UID counts zero attachments and is reported.
- It is an estimate, not `email import`'s `mail_parser` count, and the two are not guaranteed to agree.

CLI-0146 §7 refused this pass for `email import` because there it would cover the whole pending corpus at a throttled pace. Here it covers only the delta since the last ingest. The exception is the first scan of an account that has never been imported, where the delta is everything. So the count is capped: **above `ATTACHMENT_COUNT_CAP = 5,000` new UIDs in one identity, attachments are not counted at all** and every affected row reads `unknown`. The count is also `unknown` if reopening a mailbox to count it fails.

### 6. `scan-status.tsv`

One row per mailbox, then a `*` total row, **overwritten** by every scan:

```
# mailbox  uidvalidity  server_messages  ingested  new_messages  new_attachments  removed_on_server  last_scanned_at  last_in_sync_at
INBOX      1700000000   18234            18230     4             1                0                  2026-10-20T06:00:00Z  2026-10-19T06:00:00Z
*          -            40112            40108     4             1                0                  2026-10-20T06:00:00Z  2026-10-19T06:00:00Z
```

- **`last_in_sync_at`** answers "when was this last confirmed fully ingested?". It is the last scan at which `new_messages` was 0. A scan with nothing new sets it to now. Otherwise it is carried forward from the previous snapshot, which the scan downloads first (`status::carry_forward`, pure). It is `-` if never. The `*` row is treated the same way, so it answers the question for the whole account.
- **The file is TSV**, matching every checkpoint and report in this crate. `new_attachments` is a number or `unknown`, and `uidvalidity` is `-` on the total row.
- **It is the latest snapshot only.** Each run's full table also goes into `email-scan-report.txt` (each row prefixed with its identity, then one `# failed` line per identity that failed), and that report is uploaded to `--debug-log-bucket` under CLI-0100's per-run prefix. That is where the history lives.

### 7. `email import` reads and writes the ledger (amends CLI-0146 §1 and §6)

- **The ledger is the default.** `--ledger-bucket` is mandatory unless `--legacy`. `gather_pending` loads the ledger (or seeds it from `--destination`, §4) and computes pending as server UIDs minus `ingested_uids(mailbox, current UIDVALIDITY)`.
- **Each delivery is recorded twice**: the existing `.pushed` append, then a ledger row appended to the local working copy `<staging>/ingested.tsv` (`LedgerSync::append`). Both happen only after every object for that UID is confirmed pushed (CLI-0146 §5 step 6). The working copy starts as the full downloaded ledger, so every push carries the whole history, not just this run's rows.
- **The ledger is pushed every 10 minutes during the run** (`LEDGER_FLUSH_INTERVAL`), from a span-instrumented task (CLI-0141), and **once more after the workers drain**, including when a worker panicked, since whatever was delivered before that is still delivered. Each push copies a snapshot under the append lock first, so a concurrent append cannot tear the uploaded copy. A multi-day run therefore risks at most ten minutes of delivery history to a killed host.
- **A failed push** is a WARN and counts in `JobSummary.ledger_flush_failures`, outside the `FailureBreakdown` sum, since every message involved *was* delivered. **A failed final push fails the run** (exit 1), because deliveries missing from the durable ledger would be ingested again by the next run on another host. The transcript says so in those words.
- **`.pushed` gains a `UIDVALIDITY` column**, so its rows carry the ledger's key: `<mailbox>\t<uidvalidity>\t<uid>`. A pre-CLI-0164 two-field row still parses, with no `uidvalidity`. `Batch` and `PendingMailbox` gain a `uidvalidity` field so a delivery knows its generation.
- **`--legacy` is CLI-0146 exactly**: the local `.pushed` only, reset on a `UIDVALIDITY` change via the `.uidvalidity` markers. It accepts `--ledger-bucket` and ignores it, like every other job (§2).
- **Single writer per identity is assumed.** Two imports of one identity running at once would each push their own working copy, and the last one wins, losing the other's rows until it is re-ingested. A scheduler runs one import per identity; this ADR records the race rather than locking against it (Out of scope).

### 8. Observability

The CLI-0146 §8 rules apply from day one:
- Per-phase counts via `record_phase_count("email-scan", …)` for `connect`, `examine`, `bodystructure` and `state`, plus `ledger` (`loaded`/`seeded`) from the shared ledger code, and `email-import`'s new `ledger` `ok`/`failed` per push.
- `info!` lines at each identity's start and finish (with every total), at run end, and for each ledger load, seed and push.
- A WARN per ambiguous seed relpath and per unparseable `BODYSTRUCTURE`.
- `#[tracing::instrument]` on `scan_identity`.
- No `set_macro_phase`, as for CLI-0130's `transform` and CLI-0146's import.

## Consequences

- **Breaking: every scheduled invocation using `--report-bucket` fails** with an unknown-argument error until it says `--debug-log-bucket`. No alias was kept, on purpose (§2). The library-hosted `analyze-job-run` skill text should follow in a library PR plus a pin bump, if it names the flag.
- **Breaking: `email import` now needs `--ledger-bucket`** (or `--legacy`). The first ledger-mode run of an identity seeds from `--destination`, so already-delivered mail is not fetched again.
- **A message moved between mailboxes reads as new** and is ingested again under its new mailbox. That is the `(mailbox, UIDVALIDITY, UID)` key working as chosen. Message-ID identity would avoid it, at the cost of a header fetch per message (Out of scope).
- **The ledger bucket is now critical data.** Losing it means re-seeding from a destination that may already have been rewritten downstream, which under-seeds and re-ingests (§4). It deserves the same retention care as the destination.
- **Every error leans toward re-ingesting**: torn lines, ambiguous seeds, an under-seeded processed destination. An extra copy is the recoverable failure; a skipped message is not.
- **`--ledger-bucket` exists on four jobs that do not use it.** That is the price of one uniform scheduler contract, and the help text says the flag is unused for those jobs.
- **This is the first `--legacy` flag on any job.**

## Out of scope

- **How "new mail found" triggers anything, and the correlating job that reads `scan-status.tsv` to run an import batch.** Explicitly the next step, not this one. Filed per CLI-0031. ([#142](https://github.com/noisypigeon/pigeon-cli/issues/142))
- **Email deduplication.** The ledger is designed so it can happen downstream without causing re-ingestion (§3), but the dedup itself is its own decision. Filed per CLI-0031. ([#143](https://github.com/noisypigeon/pigeon-cli/issues/143))
- **Message-ID-based identity.** It would survive moves between mailboxes, but it costs a header fetch per message, and Message-IDs are missing or shared often enough to need their own rules. A permanent design choice for now, not a deferral.
- **Concurrent writers to one identity's ledger** (§7). A lock or a merge-on-push would close the race. Filed per CLI-0031. ([#144](https://github.com/noisypigeon/pigeon-cli/issues/144))
- **Retiring `--legacy`**, once nothing depends on local-only resume. Filed per CLI-0031. ([#145](https://github.com/noisypigeon/pigeon-cli/issues/145))

## Amends

- **CLI-0100**: `--report-bucket` → `--debug-log-bucket` on every job, `ReportBucketInput` → `DebugLogBucketInput`. Upload behavior unchanged.
- **CLI-0146**:
  - §1's flag set gains `--ledger-bucket` and `--legacy`.
  - §6's `.pushed` gains a `UIDVALIDITY` column, and pending is decided by the durable ledger unless `--legacy`.
  - §7's no-`BODYSTRUCTURE` decision still holds for import. Scan does run that fetch, but only over the new UIDs and capped (§5).

## Verification

- `mise run ci` clean: fmt-check, `clippy -D warnings`, and all tests (594 unit, 90 CLI).
- **`email_import/ledger.rs`** (11 tests):
  - parse/serialize round-trip;
  - lenient parsing of headers, blank lines and malformed rows;
  - `insert` keeps the earliest `ingested_at`;
  - `ingested_uids` ignores other mailboxes and older generations;
  - the seed maps keys back to their mailboxes, ignores attachments and foreign keys, skips a relpath two mailboxes share, and handles a mailbox at the identity root;
  - timestamps are zero-padded and sort chronologically;
  - `LedgerSync` appends onto the seeded copy and parses back;
  - a real `rclone` round trip against local paths: a missing object reads as `None`, and an upload then a download returns the bytes. Skipped when `rclone` is not on `PATH`.
- **`email_import/manifest.rs`**: a two-field pre-CLI-0164 row and a three-field row both load and both resume. The existing tests now write three fields.
- **`email_scan/status.rs`** (5 tests): the diff reconciles server, ingested, new and removed; the total sums rows, and one `unknown` makes the total `unknown`; `carry_forward` stamps in-sync rows, keeps the old value otherwise, and stays `None` for a mailbox never in sync; serialize/parse round-trips the total row; malformed rows are dropped.
- **`email_scan/bodystructure.rs`** (6 tests): CLI-0033's counting cases; bisection splits a batch and poisons a lone UID; bisection converges on the one poisoned UID among healthy ones; `ATTACHMENT_COUNT_CAP` is pinned at 5,000 and inclusive.
- **`email_scan/wizard.rs`**: the report prefixes each row with its identity and lists failures.
- **`tests/cli.rs`**:
  - `email --help` lists `scan`;
  - scan's flag set is present, and its five absent flags are absent;
  - scan requires `--ledger-bucket`, names an unknown ledger or destination alias, and fails fast without `rclone`;
  - import requires `--ledger-bucket` unless `--legacy`, and `--legacy` both works without it and ignores an unconfigured one;
  - import names an unknown ledger alias;
  - on all six jobs, `--report-bucket` is rejected and `--debug-log-bucket` and `--ledger-bucket` are offered;
  - a non-email job ignores an unconfigured `--ledger-bucket`.
- **Not yet verified, manually**, against a real identity and buckets:
  1. A first scan with `--destination` seeds the ledger.
  2. `email import --ledger-bucket` then delivers only what is pending.
  3. A second scan shows 0 new and stamps `last_in_sync_at`.
  4. After sending a test email, a scan shows 1 new with its attachment count.
  5. After renaming a delivered `.eml` at the destination, a scan still shows 0 new, which proves the snapshot property (§3).
  6. `pigeon.jsonl` carries `spans[].command == "job.email-scan"` on every event.
