# ADR-0140: restore the bucket name in the generated rclone config

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

ADR-0139 shipped, and the same run still did nothing:

```
job run transform --input-file-type=jpg --source source --destination destination \
  --report-bucket reports --non-interactive ...

0 transcoded, 0 copied through, 0 failed.
failures by stage: 0 pull, 0 transcode, 0 place, 0 push.
skipped: 0 already done at the destination, 0 AppleDouble sidecar(s).
pull: 0 transferred, 0 rclone error(s) reported.
```

while `rclone ls source:jpg` -- run by hand, against the operator's own
`rclone.conf` -- listed tens of thousands of files. ADR-0139's extension
fix was in the binary and was not the problem.

**`write_sandboxed_config` never wrote `BucketConfig::bucket` anywhere.**
ADR-0136 §2 emits one section per bucket-config, keyed by alias:

```
[source]
type = s3
provider = Other
access_key_id = …
secret_access_key = …
endpoint = https://s3.fr-par.scw.cloud
acl = private
no_check_bucket = true
```

An `s3` remote section has no bucket field, so the configured bucket name
(`pigeon-3gpf9n-deduplicate`, sitting in `keyring.toml` the whole time) was
simply dropped. Under that config, for `rclone`:

- `source:` is the **account root** -- a bucket listing, not a bucket;
- `source:jpg/` is **a bucket literally named `jpg`**.

Verified against the live remote with pigeon's own generated config:

| command | result |
| --- | --- |
| `rclone lsd source:` | 6 buckets, incl. `terraform-t0nh1b-state` |
| `rclone lsjson … 'source:jpg/'` | `ERROR : error listing: directory not found`, non-zero exit |
| `rclone lsjson … 'source:pigeon-3gpf9n-deduplicate/jpg/'` | 38,352 `.jpg` objects, 96.561 GiB |

ADR-0136 §2 states the reasoning that produced this: it declined "the
two-section `alias → bucket-name` indirection the terraform reference above
uses", because "aliases are already globally unique (`Store::contains_alias`,
ADR-0022), so there is no collision class to guard against by construction".
That is true, and it answers a different question than the one that
mattered -- *where does the bucket name live?* The externally-provisioned
config it was modeled on uses the indirection for exactly that reason:

```
[pigeon-3gpf9n-deduplicate]
type = s3
…
[source]
type = alias
remote = pigeon-3gpf9n-deduplicate:pigeon-3gpf9n-deduplicate
```

ADR-0138 then restored `alias:prefix/` addressing on top of a config that
could not express a prefix *within a bucket* at all, and ADR-0139 corrected
the extension filter inside an address that never resolved -- two fixes to
the right-hand side of a broken left-hand side. Both stand; neither could
have worked.

The failure was silent twice over. `Lsjson::finish` treated `"directory not
found"` in stderr as an empty listing (its ADR-0120 brand-new-destination
case, applied to every listing), and `run_transform_job` skips spawning
`rclone` entirely on an empty pull set -- so a misaddressed run was
byte-for-byte a successful no-op: 0 pulled, 0 failed, 0 rclone errors, exit
0. ADR-0139's own stderr WARNING for an empty listing fired, correctly, and
was the only signal that anything was wrong.

**Two further hazards from the same dropped bucket name, never triggered:**

- `job run rclone delete --source-bucket <alias>` resolves to `alias:` and
  runs `rclone purge alias:` -- a recursive, irreversible delete of **every
  bucket in the account**, terraform state included. The blast radius of a
  job whose entire purpose is a scoped purge was the whole account.
- `job run deduplicate` lists and pulls `alias:` (every bucket, not its
  source bucket) and pushes to `destination:`, where `no_check_bucket =
  true` has `rclone` read the first key segment as a bucket name and write
  outside the configured bucket.

## Decision

### 1. Two sections per bucket-config (`core/rclone/config.rs`)

`write_sandboxed_config` emits, per `BucketConfig`:

```
[<alias>.s3]
type = s3
provider = Other
access_key_id = <access_key_id>
secret_access_key = <secret_key>
endpoint = <endpoint>
acl = private
no_check_bucket = true

[<alias>]
type = alias
remote = <alias>.s3:<bucket>
```

The `alias` remote is the only one any job addresses; the `s3` backend
exists to be pointed at. `alias:` is the configured bucket's root again and
`alias:prefix/` a directory within it -- which is what every address in this
crate has assumed since ADR-0138, and what the deployment's own
`rclone.conf` has always provided.

The backend is keyed by **alias**, not by bucket name as the terraform
config does: aliases are unique by construction (ADR-0022), whereas two
bucket-configs may legitimately name the same bucket on different endpoints
or with different credentials, and collapsing those onto one bucket-named
section would hand one of them the other's keys.

Alias strings are unvalidated and unbounded, so a configured alias *can*
collide with another's derived `<alias>.s3` backend name. That returns an
`Err` naming the collision rather than silently misrouting one alias into
another's bucket -- the one collision class this naming scheme introduces,
guarded explicitly instead of argued away.

`RemoteTarget`, `address()`, every call site, both `source_bucket`/
`destination_bucket` metric label values, and `deduplicate`'s path-keyed
`.processed` checkpoint (ADR-0133) are **byte-identical** after this change.
That is the reason to fix generation rather than the address: the
alternative -- `address()` emitting `alias:<bucket>/<prefix>/` -- would
re-key `deduplicate`'s checkpoint and change label values one ADR after
ADR-0133 already did both.

Unchanged: `0600` permissions (ADR-0135), never reading an existing file at
the path, no `RCLONE_CONFIG` consultation, one regeneration per job run.

### 2. A failed listing stops being an empty listing (`core/rclone/command.rs`)

`Lsjson` gains `missing_ok: bool` (default `false`). `finish` returns the
real error unless it is set; the `"directory not found"` stderr match is
re-homed behind the flag, unchanged (issue #63's version-fragility stays
open, now on one call site instead of three).

- `transform::destination::list_existing_filenames` → `true`. ADR-0120's
  brand-new-destination case is real and this is the only listing that has
  it.
- `transform::destination::list_source_relative_paths` → `false`.
- `deduplicate::manifest`'s source listing → `false`.

This costs nothing in false positives on S3: a missing *prefix* inside an
existing bucket exits 0 with `[]` (verified against the live remote), so
`"directory not found"` there only ever means the bucket itself is wrong --
i.e. a misconfiguration, which is precisely the case that must not return
"empty". ADR-0139's empty-listing WARNING stays: it still covers a source
that resolves but holds nothing matching.

## Consequences

- **Every rclone-addressed job is scoped to its configured bucket again**:
  `transform`'s pull/push/listings, `deduplicate`'s listing/pull/push, and
  `rclone copy`/`delete`'s operands. The two never-triggered hazards above
  are closed by construction, not by a guard.
- **A misaddressed source now fails the run** instead of exiting 0 having
  done nothing. Intended: that silence cost two full debugging rounds and
  hid this bug behind ADR-0139's.
- **The generated config is now behaviorally equivalent to the deployment's
  terraform-provisioned one** for every pigeon-managed bucket, so an address
  that works in an operator's own `rclone ls` works under pigeon. That
  equivalence is the point; ADR-0136's sandboxing (only pigeon-managed
  buckets, regenerated per run, never merged) is untouched.
- The config file doubles in section count. It stays one well-known,
  inspectable artifact; `grep '^\[' rclone.conf` now shows `[x]` and
  `[x.s3]` per bucket.
- An alias ending in another alias's name plus `.s3` now fails
  `write_sandboxed_config`, and therefore fails every rclone-backed job,
  until renamed. No existing alias in this deployment is affected.
- A `transform` run that previously "succeeded" doing nothing will now do
  real work against a large bucket -- the same caveat ADR-0139 recorded,
  for the same reason, and this time the address actually resolves.

## Out of scope

- Validating alias strings at `pigeon keyring add` time (a charset, a
  length, a reserved-suffix rule). The collision is caught at config
  generation, which is the only place the derived name exists.
- Issue #63's stderr-substring fragility. Narrowed to one call site here,
  not fixed -- `rclone` has no machine-readable "not found" signal for
  `lsjson` short of parsing exit codes it does not differentiate.
- A preflight that verifies each addressed bucket exists before a job's
  first real operation. The source listing's hard failure now covers the
  case that actually occurred.
- `deduplicate`'s whole-bucket-only addressing (ADR-0138 §2). Its alias now
  resolves to the right bucket root; carrying a prefix still needs the
  checkpoint rework ADR-0138 declined.

## Amends

ADR-0136 (its single-section config generator, which had nowhere to put
`BucketConfig::bucket`, and its explicit rejection of the two-section
indirection) and ADR-0120 (its "a path that doesn't exist is empty"
handling, now opt-in per listing). Each gets a short forward-pointing
"Amendment" section, per this repo's convention.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `config.rs`: the exact two-section file for one bucket-config (full-string
  equality, not `contains`); two aliases naming the same bucket on different
  endpoints each get their own backend with their own credentials; an alias
  colliding with a derived backend name returns `Err`; zero bucket-configs
  still writes an empty file; `0600` and no-merge tests unchanged.
- `command.rs`: `Lsjson::finish` on a real captured `"directory not found"`
  stderr is `Ok(empty)` with `missing_ok`, `Err` naming both the path and
  rclone's own stderr without it; a non-missing failure
  (`SignatureDoesNotMatch`) errors either way.
- `destination.rs` (real `rclone` subprocess): a nonexistent source now
  errors and names the path; a nonexistent destination still lists empty.
- Live remote, pigeon's own generated config: `lsjson … 'source:jpg/'`
  returns real entries where it previously errored, and the whole original
  failing command reaches a non-zero `files_to_pull`.
