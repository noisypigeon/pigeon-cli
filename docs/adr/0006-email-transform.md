# ADR-0006: `pigeon email transform`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

`pigeon email transform` is still a stub (`src/commands/email.rs`'s `transform()` prints `Not Yet Implemented`). ADR-0001 fixed the output naming scheme and folder shape (`YYYY-MM-DD-${sanitized-subject}.md`, kebab-case, a shared `attachments/` folder) but explicitly deferred "Frontmatter and Taxonomy" to "a future ADR." ADR-0005 fixed transform's actual *input* shape: one raw `.eml` file per message under `<sink-directory>/<mailbox-path>/<uid>.eml`, with mailbox names left in IMAP's raw wire-form (modified UTF-7) encoding, explicitly deferred to transform. This ADR decides both: how transform parses `.eml` input into Markdown, and how frontmatter/taxonomy work.

## Decision

### Parsing and conversion crates

- [`mail-parser`](https://docs.rs/mail-parser) (Stalwart Labs) parses each `.eml` file. It's zero-copy, fully RFC 5322/2045-2049/2047 compliant, decodes 41 character sets, and — unlike libraries that expose the raw nested MIME tree — gives a flat, human-friendly view: `MessageParser::default().parse(bytes)` → `Message`, with `.subject()`/`.from()`/`.to()`/`.date()` for headers, `.body_text(n)`/`.body_html(n)` for body parts, and `.attachment(n)`/`.attachment_name()` for attachments. One crate covers parsing entirely; no separate RFC 2047 or base64/quoted-printable decoder is needed.
- [`htmd`](https://docs.rs/htmd) (turndown.js-inspired, built on `html5ever`) converts a `text/html` body part to real Markdown — `htmd::convert(html) -> String`. When a message has a genuine `text/html` part, it's converted via `htmd` and used as the Markdown body **even when a `text/plain` alternative also exists** — the HTML part is often the richer, more complete one (proper links, formatting), while an accompanying plain-text part is frequently a lossy auto-generated fallback. Plain-text-only messages use the plain text as-is; no conversion needed.
- `chrono`, already in the dependency tree transitively (`async-imap` depends on it for `INTERNALDATE` parsing), gets promoted to a direct dependency for formatting the `Date` header — no new dependency, just a declared one.
- No YAML crate. `serde_yaml` was archived/deprecated in 2024, and its maintained successors (`serde-saphyr`, `yaml-rust2`, etc.) are all built for round-tripping arbitrary YAML — more than `pigeon` needs, which is one-directional: emit a small, fixed set of scalar/list fields, never parse YAML back. The `---\nkey: value\n---` frontmatter block is hand-written directly, the same "no premature abstraction" style already used for the hand-rolled `.uidvalidity` marker in `src/sink.rs`.

### Naming scheme (reusing ADR-0001, not reinventing it)

- **Identity directory**: `<local>-<domain-with-dots-as-hyphens>`, e.g. `jane.doe@gmail.com` → `jane-doe-gmail-com` (ADR-0001's literal example). This is a different shape than `identity::sanitize_alias` (which drops the domain entirely, e.g. → `jane-doe`), so transform derives its own identity-directory name by applying the same reusable `identity::sanitize_segment` helper (already used for aliases and, per ADR-0005, mailbox names) to the whole address rather than just the local part.
- **Markdown filename**: `YYYY-MM-DD-${sanitized-subject}.md` — date from the `Date` header, subject sanitized via the same `sanitize_segment`.
- **Collision handling** (not addressed by ADR-0001's example): two messages landing on the same computed filename get a deterministic `-2`, `-3`, ... suffix before the extension. The same rule applies to colliding attachment filenames.

### Folder structure: flat

Every transformed message lands directly under `<identity-dir>/` — no per-mailbox subfolder, matching ADR-0001's literal example. Sink keeps its own per-mailbox subfolders (ADR-0005); transform reads across all of them but doesn't mirror that shape into its output. The mailbox a message came from becomes a tag instead of a folder (below) — this is also the direct answer to "how do we do taxonomy cross-cuts": a flat structure can only encode one axis (identity) as a folder, so every other dimension (mailbox, sender, time) has to live in metadata rather than the file tree, and metadata can encode several independent dimensions at once without duplicating files or forcing a single "true" folder for a message that has more than one relevant category.

### Taxonomy cross-cuts via namespaced frontmatter tags

Namespaced tags (`category/value`) in YAML frontmatter, the same convention tools like Obsidian use for nested tags, so each axis stays independently filterable:
- `mailbox/<sanitized-mailbox-path>` — from ADR-0005's sink output structure, e.g. `mailbox/archive/2020`.
- `identity/<alias>` — which authenticated identity this came from (relevant once multiple identities' transforms are browsed side by side).
- `sender/<sender-domain>` — from the `From` header's address domain.
- `year/<YYYY>` — a coarse time-based cross-cut, cheap to derive from the same `Date` header already driving the filename.

### Frontmatter schema

```yaml
---
from: "Jane Doe <jane.doe@example.com>"
to: "first.last@example.com"
subject: "Hello, World!"
date: 2024-01-26T09:15:00+00:00
tags:
  - mailbox/inbox
  - identity/first-last
  - sender/example-com
  - year/2024
attachments:
  - attachments/2024-01-26-hello-world-bingo-sheet.pdf
source: ../sunk/inbox/482.eml
---
```

`subject` is the raw, unsanitized subject line — the sanitized form only exists in the filename. `source` is a relative path back to the original `.eml` sink produced, so a bad transform can be debugged or redone without re-sinking from the server.

### Attachments organization

One shared `attachments/` folder per identity (not per-message) — ADR-0001's own example already establishes this. Each attachment file is named with the same `date-subject` prefix as its parent message plus the original filename (e.g. `2024-01-26-hello-world-bingo-sheet.pdf`), and collisions (two attachments landing on the same computed name, whether from one message with two same-named files or two messages sharing a `date-subject` prefix) get the same `-2`, `-3`, ... suffixing rule as Markdown filenames.

### `--mbox-to-markdown` no longer matches sink's output

`Transform`'s existing `--mbox-to-markdown` flag (from ADR-0002's scaffold) is now misnamed: ADR-0005 committed sink to one `.eml` file per message, never `.mbox`. This ADR doesn't change `src/cli.rs` — that's implementation, not decision — but flags the mismatch explicitly so a future implementation amends the flag (e.g. to `--eml-to-markdown` or drops the format qualifier entirely, since `.eml`-to-Markdown is the only format transform will support per this ADR) rather than silently keeping a name that no longer describes what it does.

### New dependencies

`mail-parser`, `htmd`, `chrono` (promoted from transitive to direct).

## Consequences

- `transform` becomes implementable against this design: parse `.eml` → Markdown with YAML frontmatter, flat per-identity output, namespaced tags for cross-cutting taxonomy.
- Downstream tooling (a future search/query command, or any Markdown-aware PKM tool like Obsidian) can rely on the tag taxonomy directly — `pigeon` itself doesn't need to build a query layer for the file tree to be useful.
- `src/cli.rs`'s `--mbox-to-markdown` flag name is flagged as needing a rename during implementation, not fixed by this ADR.

## Out of scope

- Decoding mailbox names' modified-UTF-7 encoding into human-readable tags (the `mailbox/...` tag uses the same raw sanitized form sink already produces; prettifying it can follow later without changing this ADR's taxonomy shape). ([#8](https://github.com/noisypigeon/pigeon-cli/issues/8))
- Any query/search command over the transformed output. ([#9](https://github.com/noisypigeon/pigeon-cli/issues/9))
- Deleting or modifying sink's `.eml` files — transform is read-only over its input, the same spirit as sink's read-only IMAP guarantee (ADR-0005).
