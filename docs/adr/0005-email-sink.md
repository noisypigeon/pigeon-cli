# ADR-0005: `pigeon email sink`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

`pigeon email sink` is still a stub (`service/pigeon-cli/src/commands/email.rs`'s `sink()` prints `Not Yet Implemented`). ADR-0001 named its rough shape (`pigeon email sink first-last --directory /tmp/first`) and said the output feeds a later `transform` stage that converts "MBOX and EML files" to Markdown, but never specified sink's actual download mechanics. ADR-0003 deliberately left `credentials::get_secret` unwritten, noting "nothing reads credentials back until sink exists" — sink is that consumer. This ADR decides how sink actually connects, downloads, and stores mail, without mutating anything on the server.

## Decision

### Identity selection

`Sink`'s `alias` argument becomes `Option<String>` (a documented amendment to ADR-0002's original `alias: String` shape, the same kind of change ADR-0003 made to `Authenticate`). When omitted, `sink` lists `identity::Store`'s aliases via `dialoguer::Select` — the same pattern `Provider::prompt_select()` already established in `service/pigeon-cli/src/provider.rs`. Exactly one stored identity auto-selects without prompting; zero identities is a hard error pointing the user at `authenticate`.

### Connecting

`service/pigeon-cli/src/imap_client.rs`'s `verify_login` connects, logs in, and logs out immediately — it exists only to validate a credential during `authenticate`. `sink` needs one `Session` held open across many `list`/`examine`/`uid_fetch` calls across every mailbox, so `imap_client` gains a second function that returns a live, logged-in `Session` instead of closing it. `sink`'s entire multi-mailbox download runs inside one `tokio` `block_on`, the same single-runtime-per-command shape `verify_login` already uses, just scoped to the whole operation instead of one login round-trip.

The secret itself comes from a new `credentials::get_secret(alias) -> Result<String, String>`, wrapping `keyring::Entry::get_password`.

### Read-only enforcement

Every mailbox is opened with `Session::examine(mailbox)`, never `select()` — `EXAMINE` (RFC 3501) is a server-enforced read-only mode; the server itself rejects flag/expunge mutation for the session's duration. Every fetch uses a `BODY.PEEK[]` query, never `BODY[]`/`RFC822` — `PEEK` reads the full raw message without setting the `\Seen` flag. Together these are belt-and-suspenders: `EXAMINE` makes mutation impossible even by accident, `PEEK` means `pigeon` never attempts one in the first place. Nothing is ever deleted or expunged, locally or remotely.

Messages are fetched by UID (`Session::uid_fetch`), not sequence number — sequence numbers shift as a mailbox changes over time; UIDs are stable for as long as the mailbox's `UIDVALIDITY` holds, which resume (below) depends on.

Mailboxes are enumerated with `Session::list(None, Some("*"))`. Any `Name` carrying the `\Noselect` attribute (a pure hierarchy node, not a real mailbox) is skipped, since it can't be `EXAMINE`d.

### Output format and layout

Sink writes one `.eml` file per message — the raw bytes from `Fetch.body()`, verbatim, no framing or escaping needed (unlike an mbox stream, which would require mboxrd `>From `-quoting; skipping that entirely is a concrete simplicity win from this choice). Attachments are MIME parts inside that same raw message, so they arrive automatically; unpacking them into their own files is `transform`'s job, not sink's, matching ADR-0001's taxonomy example where attachments only appear in `transform`'s output.

Layout: `<directory>/<sanitized-mailbox-path>/<uid>.eml`, one subdirectory per IMAP mailbox. The IMAP hierarchy delimiter (`Name::delimiter()`, typically `/` or `.`) maps onto nested directories, e.g. mailbox `Archive/2020` → `<directory>/archive/2020/`.

**Mailbox name encoding**: IMAP mailbox names use "modified UTF-7" for non-ASCII characters (RFC 3501 §5.1.3). Neither `async-imap` nor `imap-proto` decode this — `Name::name()` returns the raw wire form (e.g. `Résumé` arrives as `R&AOk-sum&AOk-`). Sink sanitizes that raw wire string into a directory name as-is: correct and collision-free, but not human-readable for non-ASCII folder names. Decoding it properly is cosmetic and left to `transform`, consistent with the taxonomy/naming work ADR-0001 already scoped there. This is a known, documented limitation, not a silent gap.

### Resume mechanics

Resume is in scope for v1: re-running `sink` into the same directory skips messages already downloaded rather than requiring a fresh empty directory every time.

- **Filenames are the resume index — no separate database.** Before fetching bodies for a mailbox, a cheap `uid_fetch("1:*", "(UID)")` lists every UID the server currently has for it; diffing that against the `<uid>.eml` files already on disk gives exactly the UID set still needed. Only those are fetched with `BODY.PEEK[]`. An interrupted sink is therefore always safe to just re-run.
- **`UIDVALIDITY` is the one piece of state that must be tracked.** `examine()`'s response exposes `Mailbox.uid_validity: Option<u32>`. UIDs are only stable within one `UIDVALIDITY` epoch for a mailbox; if a mailbox is ever rebuilt server-side, `UIDVALIDITY` changes and old local UIDs may now name entirely different messages. Sink writes a `.uidvalidity` marker file into each mailbox's directory; if the server's current value doesn't match what's on disk, that mailbox's directory is treated as stale and fully re-fetched, rather than risking silently mismatched UID/message pairs.
- No mailbox is ever deleted locally if it disappears from the server between runs — resume only ever adds files, extending the "leave things intact" principle to the local archive as well as the server.

### Progress reporting

Picks up ADR-0001's "progress bar or % for long-running operations," which ADR-0002 and ADR-0003 never addressed. `sink` — potentially downloading years of mail — is exactly the operation that was about. `indicatif` drives one progress bar per mailbox, sized from `Mailbox.exists` and pre-filled for whatever resume already found on disk. It's a natural addition dependency-wise: same `console-rs` family as the already-installed `dialoguer`/`console`.

### New dependencies

`indicatif`.

## Consequences

- `sink` becomes implementable against this design: a real, credential-backed, read-only, resumable download of every mailbox into `.eml` files.
- `Sink`'s `alias` argument shape changes from required to optional, extending ADR-0002/ADR-0003's precedent of documented CLI-surface amendments as real behavior lands.
- `transform` (converting sunk `.eml` files to Markdown, taxonomy/file naming, attachment extraction, and decoding modified-UTF-7 mailbox names into readable folder names) remains entirely its own future ADR — nothing here decides that.
- Risk: a mailbox with an extremely large message count means a very long-running `uid_fetch("1:*", "(UID)")` diff and download; no batching/pagination strategy is decided here, deferred until it's a demonstrated problem.

## Out of scope

- `transform` itself.
- Deleting, expiring, or otherwise mutating local files sink has already written (beyond the documented stale-`UIDVALIDITY` re-fetch case). ([#7](https://github.com/noisypigeon/pigeon-cli/issues/7))
- Any server-side mutation whatsoever — enforced structurally via `EXAMINE`/`BODY.PEEK[]`, not just by convention.
