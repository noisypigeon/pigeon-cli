# ADR-0010: `pigeon remote` improvements from first real use

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

ADR-0009 shipped `pigeon remote`. Its first real run, against a DigitalOcean Spaces bucket, surfaced concrete friction:

- Several confusing `configure` attempts before values stuck — the prompt order and labels (`Remote name`, `Endpoint URL (e.g. ...)`, `Secret access key`, `Region`) didn't match how the alias/bucket/endpoint/credentials actually relate to each other, and the first attempt garbled entirely.
- A scary-looking, multi-line `AccessDenied` dump from the "list buckets with these credentials to help choose?" step, even though the overall flow still completed successfully afterward. DigitalOcean Spaces access keys are commonly scoped to a single bucket and can't call the account-level `ListBuckets` API — an expected, already-handled fallback, but the error text reads like a crash.
- An explicit request to rename/reorder `configure`'s prompts, drop the region prompt, and add the ability to see, edit, and remove already-configured remotes — today the only way to do any of that is hand-editing `remotes.toml` and separately clearing the OS keychain.

This ADR addresses all of it.

## Decision

### Rename `name` → `alias`, everywhere

CLI flags and prompts, `Remote.name` → `Remote.alias`, `remotes.toml`'s key, the OS keychain key, `Store::contains_name` → `contains_alias`, `location::Location::Remote { name, .. }` → `{ alias, .. }`. This resolves the ambiguity from an earlier exchange ("is the name the full name or the alias?") at the data-model level, not just a prompt label — there was never a separate "full name" concept for a remote the way an email address is separate from an identity's alias, so the field should just be called what it is.

### Reorder and relabel `configure`'s prompts

Exactly as requested: **Alias → Bucket Name → Endpoint URL → Access Key ID → Secret Key**. The endpoint prompt drops its inline example text (`(e.g. https://nyc3.digitaloceanspaces.com)`) — the field name is now self-explanatory enough on its own, and the example added noise without adding clarity once the ordering makes sense.

### Remove `region` entirely

From the struct, the TOML schema, and the prompt. This isn't just simplification for its own sake: `Remote.region` was captured at `configure` time and persisted, but nothing in `src/remote/client.rs` ever reads it — no builder call anywhere sets `.region(...)`. The `minio` crate resolves region per-bucket on its own. The field was dead the moment it was written, matching the user's own instinct that it's "inferred by the endpoint" anyway (true for DigitalOcean Spaces and most S3-compatible providers, which bake the region into the hostname).

### Remove the inline "list buckets to help choose" step from `configure`

Two independent reasons converge on the same fix:

1. The new prompt order asks for the bucket *before* any credentials exist, so there's nothing left to call `list-buckets` with at that point in the flow — the reordering the user asked for structurally removes this step, it doesn't just move it.
2. It was unreliable in practice: a bucket-scoped access key (the common case for DigitalOcean Spaces) can't call the account-level `ListBuckets` API at all, so the "helpful" step routinely produced the scary `AccessDenied` dump instead of a bucket list.

In its place: a `bucket_exists` check against the entered bucket, right before saving — mirroring `email::authenticate`'s verify-before-persist precedent (ADR-0003). Unlike account-level `ListBuckets`, checking whether one specific bucket is reachable is exactly the permission scope a bucket-restricted key is expected to have, so this is both more reliable *and* actually catches a typo'd or wrong bucket name immediately — the root cause of the multiple `configure` attempts in the first place — instead of silently saving bad config that only surfaces as a failure later, at `ls` or `copy` time. `list-buckets` remains available as its own standalone command, unchanged, for credentials that do support account-level listing.

### Concise S3 error formatting

The scary dump wasn't a pigeon formatting bug — `eprintln!("...{err}...")` already uses `Display`, and the `minio` crate's error type's own `Display` impl prints a verbose multi-line structured dump (code, message, resource, request ID, host ID, bucket, object) by design, meant for debugging, not for a CLI's stderr. Every error surfaced from `remote::client`'s functions (`list_buckets`, `list_objects`, `get_object`, `put_object`, and the new `bucket_exists` check) gets reduced to one line built from the error's structured fields (its code and message) instead of the crate's full `Display` output. This is a general fix applied uniformly, not a patch on the one call site that happened to surface it this time.

### New commands: `list`, `edit`, `remove`

- **`pigeon remote list`** — lists configured remotes (alias, endpoint, bucket), mirroring `email list-identities` exactly.
- **`pigeon remote edit [ALIAS]`** — interactively updates an existing remote's bucket, endpoint, access key ID, and secret key. The current value is shown as the default for each prompt; pressing enter keeps it. The alias itself is not renameable in v1 — renaming in place would mean migrating the keychain entry under a new key, which is deferred as unneeded complexity for now. Renaming is `remove` followed by `configure` under the new alias.
- **`pigeon remote remove [ALIAS]`** — confirms, then deletes the entry from `remotes.toml` and its secret from the keychain.

Together these mean `remotes.toml` and the keychain entries backing it are never something a user needs to touch by hand.

## Consequences

- `configure` becomes strictly simpler — five straight prompts, no branching, no S3 call that can produce a wall of text mid-flow — while also becoming *safer*: it now verifies before saving, where before it saved unconditionally regardless of whether the bucket was even reachable.
- `region`'s removal is a pure deletion of dead surface; nothing that worked before stops working.
- Every `remote::client` error a user sees going forward is one line, not a multi-line struct dump, regardless of which command surfaced it.
- `list`/`edit`/`remove` round out remote management to the same completeness `email` already has with `list-identities` (and, now, `authenticate`'s re-run-to-overwrite gap aside, comparable lifecycle coverage).

## Out of scope

- Renaming an existing alias in place (remove + reconfigure instead). ([#15](https://github.com/noisypigeon/pigeon-cli/issues/15))
- Any change to the S3 client crate or connectivity model ADR-0009 already decided.
- Any change to `email`.
- Implementation itself — like every ADR before its own separate implementation request, this is a decision record only.
