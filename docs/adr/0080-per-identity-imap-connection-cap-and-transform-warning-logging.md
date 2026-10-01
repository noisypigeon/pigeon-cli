# ADR-0080: per-identity IMAP connection cap via keyring, and `transform.rs` warning logging

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-28.
- **Status**: Accepted.

## Context

`docs/report/0001-email-sync-pull-transform-log-analysis.md` (written by the
`analyze-job-run` skill, ADR-0078) analyzed a real `job run email-sync` failure
(`exit_code=1`, 7 identities, 3h44m) and a separate `job run pull-transform` `SIGKILL`. Both
findings are CONFIRMED, with numbers reconciling exactly against captured transcripts. The
report ends with four "Next actions (candidates, not yet decided)". This ADR decides and
implements the two that are actionable now; the other two are filed as tracked GitHub issues
per ADR-0031, not silently dropped. (Identity/mailbox names in the report -- e.g. the
affected identity's placeholder alias -- are explicitly fictional stand-ins; nothing here is
keyed to a specific real alias.)

1. `--concurrency 16` combined with ADR-0071's flat `--max-connections-per-identity` default
   (6, one value applied to every identity in the run) was still too many simultaneous IMAP
   sessions for one identity's provider specifically (`OVERQUOTA` / "Too many simultaneous
   connections"), which exhausted ADR-0071's batch retry budget and dropped 118,962 UIDs.
   Every *other* identity in the same run was fine at the same cap -- this is identity-
   specific, not a global-tuning problem, so the fix is an identity-specific, persistent
   setting, not a per-run CLI flag.
2. The report surfaced a second, previously-invisible defect: `transform.rs`'s four
   lenient-skip `eprintln!` warnings (including "Warning: failed to read ...: No such file or
   directory") never reach the JSONL log -- confirmed by grepping the whole log file for "No
   such file"/"failed to read": zero matches. This is a consistency gap against every other
   job phase (`worker.rs`), which already logs equivalent warnings via `tracing::warn!` with
   structured fields (ADR-0033's established convention).
3. `"Validation error occurred"` (a generic S3-client error string, 64 occurrences, 5
   permanent failures in the analyzed run) needs its own reproduction against a single
   bucket-config, and the actual underlying S3 response, before a fix can be designed --
   deferred to a filed issue, not decided here.
4. The `pull-transform` `SIGKILL` root cause is genuinely open (confirmed *not*
   self-inflicted memory growth -- `mem_bytes` stayed flat 400-820MB through the last sample)
   and needs system-level correlation on a recurrence -- deferred to a filed issue, not
   decided here.

This ADR amends ADR-0071 (its per-identity connection cap becomes overridable per identity,
sourced from the keyring rather than only a job-wide flag) and ADR-0022 (`Identity` gains a
new optional field, editable via the existing `pigeon keyring modify` flow).

## Decision

### 1. `Identity` gains an optional `max_imap_connections` cap, set via `pigeon keyring add/modify`

`Identity` (`src/commands/keyring/email/identity.rs`) gains:

```rust
#[serde(default)]
pub max_imap_connections: Option<u32>,
```

-- the same `#[serde(default)]`-optional-field pattern already used for `BucketConfig`'s
`encryption_key_alias`, so existing `keyring.toml` files parse unchanged (field absent ->
`None`, no migration needed).

`add_email` and `modify_email` (`commands/keyring/wizard.rs`) each gain a new prompt step
mirroring `modify_bucket`'s existing `encryption_key_alias` shape: a
`confirm("Cap simultaneous IMAP connections for this identity?", <currently set?>)` gate,
then either an `Input::<u32>` prompt (current value as the default when already set) or
`None` to leave/clear it uncapped.

`KeyringEntry::detail()` for `Entry::Email` appends `", max N IMAP conns"` when set, so
`pigeon keyring list` surfaces a configured cap without needing `modify` to check.

### 2. `job run email-sync` consumes the per-identity cap, falling back to the existing global default

No new CLI flag, no wizard changes in `job/email_sync/`. `IdentityContext` already carries
the full `Identity` (including the new field) through to `worker::run_email_sync_job`'s
`identities: Vec<IdentityContext>` parameter. The per-identity semaphore construction changes
from `identities.iter().map(|_| ...)` to resolving each identity's own cap first:

```rust
let cap = ctx.identity.max_imap_connections
    .map(|value| value as usize)
    .unwrap_or(max_connections_per_identity);
Arc::new(Semaphore::new(concurrency.min(cap).max(1)))
```

ADR-0071's existing `--max-connections-per-identity` flag/`DEFAULT_MAX_CONNECTIONS_PER_IDENTITY`
is unchanged and still applies to every identity that hasn't set its own
`max_imap_connections` -- this is a per-identity *override* of that default, not a
replacement for it.

Extending ADR-0071's batch retry budget (`BATCH_RETRIES`/backoff) instead of, or in addition
to, this cap is explicitly *not* done here -- deferred, mirroring ADR-0071's own "deferred
until proven insufficient" posture for its per-provider-tuning bullet. A tighter
identity-specific connection cap is the direct fix for what the report actually showed;
retrying more before dropping a batch doesn't help if every retry still opens too many
connections.

### 3. `transform.rs`'s four lenient-skip warnings move to `tracing::warn!`

All four `eprintln!` call sites in `EmailTransform::transform` (unparseable UID filename,
unreadable file, unparseable message, missing `Date` header) convert to `tracing::warn!`,
matching the field convention already established in `worker.rs`: plain shorthand for
in-scope values, `%` for `Display` fields, a `step = "transform"` label, plain string
message.

`mailbox_tag(&eml_path, &self.input_root)` moves to the top of the function so every branch
can attach `identity = %self.identity.alias`, `mailbox`, `step = "transform"`,
`file = %eml_path.display()`, and (once parsed) `uid`, plus `error = %err` where an error
value exists. No behavior change -- `Ok(None)` lenient-skip semantics are unchanged, only the
warning's destination.

No `MultiProgress::suspend` wrapping is needed for these (unlike raw `eprintln!`/`println!`
per ADR-0015): `tracing::warn!` calls elsewhere in this same code path (`worker.rs`) are
already unwrapped, since `tracing` output doesn't interleave with `indicatif` progress bars
the way direct stdout/stderr writes do.

## Consequences

- A future OVERQUOTA/connection-limit incident against one identity can be worked around
  permanently (`pigeon keyring modify`) without touching every future `job run email-sync`
  invocation's flags, and without lowering throughput for every other identity.
- `pigeon keyring add`/`modify email` gains one more optional prompt step; existing
  `keyring.toml` files and existing automation that doesn't set this field are unaffected
  (defaults to `None`, i.e. today's global-only behavior).
- Every lenient-skip warning in the transform phase is now diagnosable from the JSONL log
  alone, matching every other job phase -- closing the exact gap the report flagged as making
  a future occurrence undiagnosable without a captured transcript.
- Terminal output for these four transform cases moves from stderr text to structured log
  lines; nothing currently parses the old `eprintln!` text.

## Out of scope

- Extending ADR-0071's `BATCH_RETRIES`/backoff for `connect`/`examine` batch failures -- the
  per-identity cap above is the direct fix for the incident as analyzed; revisit only if a
  future incident shows the cap alone insufficient.
- `"Validation error occurred"` S3-client error investigation (report item 3) -- needs its own ([#88](https://github.com/noisypigeon/noisypigeon/issues/88))
  reproduction against a single bucket-config and the actual underlying S3 response before a
  fix can be designed. Filed as a GitHub issue via `mise run adr-issue`.
- `pull-transform` `SIGKILL` root cause (report item 4) -- genuinely open, confirmed not ([#89](https://github.com/noisypigeon/noisypigeon/issues/89))
  self-inflicted; needs system-level correlation on recurrence. Filed as a GitHub issue via
  `mise run adr-issue`.

Implementation, the report, and this ADR land together on the same branch.
