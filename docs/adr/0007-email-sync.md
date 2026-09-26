# ADR-0007: merge `sink`+`transform` into `pigeon email sync`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

`pigeon email sink` (ADR-0005) and `pigeon email transform` (ADR-0006) are two separate, fully-implemented subcommands: `sink` downloads mail read-only into raw `.eml` files with UID-based resume; `transform` separately parses those `.eml` files into flat, per-identity Markdown with frontmatter. For the common case — "get my mail as Markdown" — that means two invocations and every message round-tripping through disk as a permanent raw copy before it's ever converted. This ADR merges both into one command, `pigeon email sync`, while keeping each phase individually reachable via a `--debug` flag for troubleshooting.

## Decision

### New command shape; `sink`/`transform` removed as standalone subcommands

```
pigeon email sync [ALIAS] --staging-dir <staging-dir> --output-dir <markdown-dir> [--debug <sink|transform>]
```

`sink` and `transform` are removed from `EmailCommands` entirely — this isn't an additive third command alongside the existing two, it's a consolidation. `--staging-dir` is the staging directory raw `.eml` files pass through (same role as `sink`'s `--directory` today); `--output-dir` is the transformed Markdown tree (same role as `transform`'s `--output` today). Naming: `sync`, the common term in mail tooling for "pull and process together" — open to revisiting on review.

### Per-UID pipeline

For each mailbox, for each UID the server reports:
1. If the UID is already recorded in that mailbox's `.processed` marker (below), skip it entirely — already fully done.
2. Else if `<uid>.eml` already exists in the staging directory (e.g. left there by a prior `--debug sink` run, or an interrupted `sync` that fetched but never got to transform), skip straight to step 4 — no re-fetch.
3. Else, fetch it fresh via IMAP, exactly as `sink` does today: `EXAMINE` (never `SELECT`), `BODY.PEEK[]` (never `BODY[]`), written to `<staging-dir>/<mailbox-path>/<uid>.eml`.
4. Transform it into Markdown (exactly as `transform` does today), written under `--output`.
5. **Verify**, then act on the result:
   - **Success**: delete the `.eml`, record the UID in `.processed`.
   - **Failure**: keep the `.eml`, do **not** record the UID as processed, report the failure. An unverified raw copy is never deleted — at that point it's the only durable record of the message.

### Verification

Structural, not a re-parse of the source: the `.md` file exists, is non-empty, and starts with the frontmatter delimiter (`---`); every attachment path listed in that frontmatter's `attachments:` list exists on disk with nonzero size. If the writes completed, verification passes — no need to re-derive anything from the now-possibly-deleted `.eml` to check it.

### The `.processed` marker file

A new per-mailbox marker, living alongside the existing `.uidvalidity` marker (ADR-0005) in the staging directory: newline-separated UIDs that have been fetched, transformed, verified, and had their `.eml` deleted. Resume no longer means "does `<uid>.eml` exist" — a missing file might mean "never fetched" or might mean "successfully finished and cleaned up," and those need to be distinguishable. It now means "is this UID in `.processed`."

### Frontmatter schema amendment: `source:` → `uid:`

ADR-0006's `source:` field was a relative path back to the raw `.eml`. That file is now routinely deleted, so the field is amended: `uid:` (the raw IMAP UID, an integer) replaces `source:` in the frontmatter schema. This is more durable than a path — it survives the file being deleted — and keys a future reconciliation pass (see Out of scope) directly off the same identifier `.processed` and `.uidvalidity` already use.

### `--debug sink` and `--debug transform`: exactly today's behavior, always non-destructive

- **`--debug sink`**: fetch-only. Writes `.eml` files to `--directory`. Never transforms, never deletes, never touches `.processed`. This is `sink` (ADR-0005) unchanged, reachable as a flag instead of a subcommand — including for anyone who deliberately wants the old permanent-raw-archive behavior back: run `sync --debug sink` and simply never run the default flow to clean those files up.
- **`--debug transform`**: transform-only. Reads whatever `.eml` files already exist in `--directory`, writes Markdown to `--output`, and does **not** delete the source `.eml` afterward, regardless of verification outcome. This is `transform` (ADR-0006) unchanged, reachable as a flag instead of a subcommand. Debug mode stays conservative on purpose — it exists for inspection, not for reproducing the default flow's cleanup behavior.

### Explicit reversal of ADR-0001's raw-preservation framing

ADR-0001 originally wanted sink to "pull all emails and attachments" as a preservation step independent of any lossy conversion — raw mail as a permanent safety net. This ADR reverses that as the *default*: raw `.eml` becomes a transient staging artifact, deleted once its Markdown transform is verified. The safety net still exists, but now requires deliberately choosing `--debug sink` and not running the default flow against that directory. This is a real trade-off, not a silent one — flagged here explicitly per the project's own governance rule about diverging from prior ADRs.

## Consequences

- The common case drops from two invocations to one, and messages no longer permanently double their storage footprint (raw `.eml` + Markdown) by default.
- Anyone who wants ADR-0001's original permanent-raw-archive guarantee back has to opt in via `--debug sink` and manage that directory themselves — it's no longer the default behavior of getting mail out of a mailbox at all.
- `service/pigeon-cli/src/sink.rs` and `service/pigeon-cli/src/transform.rs`'s core logic barely changes — `sink::run`'s per-mailbox fetch loop and `transform::run`'s per-message parse-and-render logic are still the right shapes, just called from a shared per-UID orchestrator instead of two separate command handlers. This is a refactor of *orchestration*, not a rewrite of either phase.
- `credentials::get_secret` is now needed by the default `sync` flow and by `--debug sink`, but still not by `--debug transform` (matching `transform`'s existing no-keychain-access property).

## Out of scope

- Automatically detecting and cleaning up stale Markdown files after a `UIDVALIDITY` change (ADR-0005's staleness case already handles this for the *raw* side; the transformed side has no cheap equivalent since Markdown lives in a flat per-identity folder, not a UID-addressable one). The new `uid:` frontmatter field makes a future reconciliation pass possible; building one is deferred, same spirit as ADR-0005 deferring fetch batching. ([#10](https://github.com/noisypigeon/pigeon/issues/10))
- Any change to `authenticate` or `list-identities`.
