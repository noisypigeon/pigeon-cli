# ADR-0065: bisect UID FETCH batches to isolate unparseable BODYSTRUCTURE messages

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-27.
- **Status**: Accepted.

## Context

Running `mise run pigeon job run email-sync` crashed the entire multi-identity job:

```
Error: failed to fetch message sizes in 'Z-History/9436-7265 Quebec Inc/digitalelunaire@gmail.com': io: Error(Error { input: [...], code: TakeWhile1 }) during parsing of "* 1193 FETCH (UID 1502 RFC822.SIZE 25127 BODYSTRUCTURE (...))..."
```

**Root cause, confirmed by reading the actual vendored crate source**
(`~/.cargo/registry/src/index.crates.io-*/imap-proto-0.16.7/src/parser/rfc3501/body_structure.rs`),
not guessed:

```rust
fn body_type_message(i: &[u8]) -> IResult<&[u8], BodyStructure<'_>> {
    map(
        tuple((
            tag_no_case("\"MESSAGE\" \"RFC822\""),
            ...
```

`body_type_message` — the only parser branch that knows how to consume an
embedded-message body part's *extended* shape (nested `ENVELOPE` + nested
`BODY` + a line count) — hardcodes the literal `"MESSAGE" "RFC822"`. `body()`'s
dispatch is `alt((body_type_text, body_type_message, body_type_basic,
body_type_multipart))`, so any *other* `MESSAGE` subtype — `GLOBAL` (RFC
6532, internationalized/EAI mail), `DELIVERY-STATUS` or
`DISPOSITION-NOTIFICATION` (RFC 3462/3798 — ordinary bounce and read-receipt
messages) — fails to match `body_type_message` and falls through to
`body_type_basic`, which expects a flat `body_fields` ending in a plain
octet-count number. The real data has a nested `ENVELOPE` there instead; the
mismatch cascades and fails deep in a nested parser expecting digits —
exactly the `TakeWhile1` error observed. The actual message that triggered
this is a delivery-status bounce containing a `MESSAGE`/`GLOBAL` part.

**Confirmed no upstream fix exists to pull in**: `cargo search` shows
`imap-proto = "0.16.7"` and `async-imap = "0.11.3"` as the latest published
versions of both — exactly what `Cargo.lock` already pins. A version bump
doesn't fix this.

**Confirmed the failure's blast radius via `async-imap-0.11.3/src/parse.rs`**:
`uid_fetch`'s stream decodes the whole currently-buffered response in one
pass (`parse_fetches`); a hard parse error anywhere in that pass fails the
*entire* batched `UID FETCH` command with zero entries yielded — matching
the observed `0/37` (zero progress in that mailbox) before the crash. This
is not a rare edge case: any mailbox containing a bounce, a read receipt, or
an internationalized embedded message can trigger it.

**Where this lives**: `src/commands/job/email_sync/manifest.rs`'s
`pull_manifest` issues one `session.uid_fetch(&uid_set, "(UID RFC822.SIZE
BODYSTRUCTURE)")` for *every* pending UID in a mailbox at once, then
propagates any fetch/parse error with `?` — which currently kills the whole
job run across every identity, not just the one affected mailbox.

**Why it's safe to be lossy for just the bad message**: `ManifestEntry` is
already documented in this file as a `BODYSTRUCTURE`-derived *estimate* —
"not the authoritative count `transform_one`'s `mail_parser` pass produces
later" (ADR-0032/0033/0034) — feeding only the wizard's pre-run summary
display. The real fetch/transform/verify pipeline re-parses each message's
actual content independently later and never reads `ManifestEntry` back. A
wrong-but-harmless `0`/`0` placeholder for one skipped UID is an acceptable
cost; losing every other message's real estimate in the same mailbox
(today's behavior) is not.

## Decision

### Bisection retry in `pull_manifest`

On a fetch/parse error, split the failing UID batch in half and retry each
half as its own `UID FETCH` command, iteratively narrowing (a work-stack of
UID slices, not recursion — avoids needing a new dependency for recursive
`async fn`) until the specific poisoned UID(s) are isolated to a batch of
one. A UID that still fails in isolation is the confirmed culprit: skipped
with a logged warning, given a placeholder `ManifestEntry { size: 0,
attachments: 0 }` instead of blocking the rest of the mailbox.

The retry loop is factored as a small helper generic over the fetch
operation itself (a closure), decoupled from the concrete `ImapSession`
type — this is what makes it unit-testable at all without a live or mocked
IMAP server: a test supplies a fake fetch closure that fails for chosen
UIDs, and asserts the real ones still come back correctly and only the bad
ones get placeholders. `pull_manifest` becomes a thin wrapper passing the
real `session.uid_fetch(...)` call as that closure.

### No IMAP session reconnect

Each retry is a brand-new `UID FETCH` command on the same still-open
connection. The previous command's bytes were fully received — only
*parsing* them failed — so the connection itself should stay usable for the
next command. **Not verified against a live server**; if a real run shows
the connection desynced after a caught failure, the fallback is a full
reconnect (`connect_and_login`) — deliberately deferred unless proven
necessary, since it would require threading credentials down into
`manifest.rs`, which its current signature doesn't have.

### Not patching/forking `imap-proto`

Generalizing `body_type_message` to any `MESSAGE` subtype is the more
"correct" long-term fix, but means maintaining a `[patch.crates-io]`
override or a fork until an upstream release picks it up — real ongoing
maintenance cost for what application-level bisection already resolves.
Filing an upstream issue is worth doing separately, not part of this ADR.

## Consequences

- A mailbox with one of these messages now gets accurate manifest data for
  every *other* message in it, with only the specific culprit(s) getting a
  `0`/`0` placeholder — instead of the whole mailbox (and, per the observed
  crash, the whole job across every identity) failing outright.
- Worst case (many bad messages in one mailbox) means more `UID FETCH` round
  trips than today's single bulk call, bounded by `O(log2(batch size))` per
  bad UID found — zero extra cost on the common, no-bad-message path.
- The wizard's `ATTACHMENTS`/size estimate slightly undercounts for any
  skipped UID — an accepted, already-approximate number per ADR-0032/0033/0034.

## Out of scope

- Patching/forking `imap-proto`, or filing the upstream issue.
- Reconnecting the IMAP session after a caught failure — deferred unless
  proven necessary during implementation.
- Any change to the fetch/transform/verify pipeline — unaffected, since it
  never reads `ManifestEntry` back.
