# ADR-0020: generic transform/dedup primitives in `dataops`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.

## Context

`transform` and `dedup` move from `email` into `dataops`, genuinely genericized rather than bare-relocated, so future non-email `dataops` transforms can reuse the same machinery — realizing the intent `dataops`'s own name already signaled when ADR-0017 renamed it from `remote`.

A precise, line-by-line inventory of `src/email/transform.rs` and `src/email/dedup.rs` splits cleanly into two categories.

**Genuinely generic — no email concept baked in, or only in naming/docs:**
- `dedup::ContentIndex` (load/check/commit against a hash→relative-path file) — the struct and its methods are already 100% generic; only its two associated consts (`MESSAGE_HASHES`, `ATTACHMENT_HASHES`) are email-specific, and callers already pass an arbitrary filename to `load()` regardless.
- `dedup::amend_frontmatter_for_duplicate` — the actual algorithm (parse a `---`-delimited frontmatter block as lines, find/insert a named list field's entry, find/create a `<tag>#<id>`-keyed cross-reference list) doesn't care what the tag/id semantically mean. Only its parameter names (`mailbox_tag: &str, uid: u32`) and doc comments are email-flavored.
- `transform::unique_path` — pure filesystem collision-avoidance, no email concept at all.
- `transform::sanitize_attachment_name` (plus `truncate_preserving_extension`/`truncate_at_char_boundary`/`MAX_ATTACHMENT_NAME_LENGTH`) — "make an untrusted string safe as a filename, preserving its extension, length-capped" has nothing email-specific about it despite the name.
- `transform::yaml_quote` — generic YAML scalar escaping.
- `transform::visit_dir`/`find_eml_files`'s directory-walking core — and this is where a concrete, additional consolidation opportunity shows up: there are already **three** separate, near-identical recursive-directory-walk implementations in this codebase — `email/transform.rs::visit_dir`, `email/sync.rs::visit_dir` (added by ADR-0019), and `dataops/commands.rs::visit_dir` (original, ADR-0009). This refactor is a natural place to collapse them into one.

**Irreducibly email-specific — stays in `email`:**
- `TransformedMessage`/`TransformSummary` (result types shaped around "messages"/"attachments"), `transform_one`/`run` (the actual `mail_parser`-based EML parsing orchestration), `verify_transformed` (tied to `TransformedMessage`'s shape), and `format_address`/`mailbox_tag`/`sender_domain_tag`/`format_date_prefix`/`render_frontmatter` (email's specific frontmatter field set: from/to/subject/date/tags/attachments/uid).

## Decision

### New `src/dataops/dedup.rs`

`ContentIndex` moves as-is, minus its two email-specific associated consts (`MESSAGE_HASHES`/`ATTACHMENT_HASHES`) — callers now supply their own filename to `load()`, which the API already supported. `amend_frontmatter_for_duplicate` moves with its parameters renamed to domain-neutral terms (`mailbox_tag: &str, uid: u32` → `tag: &str, occurrence: u32`) and its doc comments generalized; the algorithm and on-disk `<tag>#<occurrence>` format are unchanged byte-for-byte.

### New `src/dataops/transform.rs`

`unique_path`, a renamed `sanitize_filename` (was `sanitize_attachment_name`, generalized doc comment) with its truncation helpers and a renamed `MAX_FILENAME_LENGTH`, `yaml_quote`, and a new canonical `collect_files(dir: &Path) -> Result<Vec<PathBuf>, String>` (recursive walk, sorted, missing directory treated as empty — matching `email/sync.rs`'s more lenient existing contract) backed by one `visit_dir`.

### The three duplicate `visit_dir` implementations collapse to one

`email/transform.rs`'s `find_eml_files` becomes a thin wrapper: call `dataops::transform::collect_files`, then filter to `.eml`. `email/sync.rs`'s `collect_output_files` becomes a thin wrapper over the same function (it already has matching missing-dir-is-empty semantics, so this is closer to a direct delegation). `dataops/commands.rs`'s `collect_local_files` keeps its own single-file-vs-directory-vs-missing branching (real, different semantics — it errors on a missing path, others don't) but delegates the actual recursive walk to `dataops::transform::collect_files` instead of its own copy of `visit_dir`.

### `email/transform.rs` keeps every irreducibly email-specific piece

Now consuming `dataops::dedup::{ContentIndex, amend_frontmatter_for_duplicate}` and `dataops::transform::{unique_path, sanitize_filename, yaml_quote, collect_files}`. It defines its own `pub(crate) const MESSAGE_HASHES_FILE`/`ATTACHMENT_HASHES_FILE` (`".message-hashes"`/`".attachment-hashes"`) — the natural new home for the two consts dropped from `ContentIndex`, since `transform.rs` is where they're conceptually anchored (ADR-0012).

### `email/sync.rs`

Updates its `ContentIndex` import path to `dataops::dedup`, and references `transform::MESSAGE_HASHES_FILE`/`ATTACHMENT_HASHES_FILE` in place of the old `ContentIndex::MESSAGE_HASHES`/`ATTACHMENT_HASHES` associated consts.

### `src/dataops/mod.rs`

Gains `pub mod dedup;` and `pub mod transform;`.

No on-disk format, CLI surface, or observable behavior changes anywhere — every algorithm, file format, and frontmatter shape stays byte-for-byte identical. This is purely an internal reorganization plus a genericization of already-generic-shaped code, confirmed file-by-file above rather than assumed.

## Consequences

- Three near-duplicate recursive-directory-walk implementations collapse into one, a concrete win beyond the headline ask.
- `email`'s dependency on `dataops` deepens: previously only the optional `--remote-output` upload path (ADR-0011) depended on `dataops`; now core transform/dedup *machinery* does too, unconditionally. Worth stating plainly — `email` can no longer build without `dataops` compiling, even for a user who never touches `--remote-output`/`dataops bucket-config`. This is a real shift from ADR-0008's original "independent siblings" framing, not a violation of it (ADR-0008 always allowed shared foundations), but undocumented drift is exactly what this project's ADR convention exists to avoid, so it's named here explicitly.
- `dataops` becomes genuinely generic infrastructure a second, non-email data source could build a transform+dedup pipeline against — realizing what ADR-0017's rename already signaled by name alone.
- No behavior change for any existing `pigeon email` command.

## Out of scope

- Moving `TransformedMessage`/`TransformSummary`/`verify_transformed`/the EML-parsing orchestration itself into `dataops` — these stay genuinely email-specific, per the inventory above.
- Any change to `dataops::client`'s S3 upload logic.
- Actually building a second, non-email `dataops`-based transform pipeline — this ADR prepares the ground for that; it doesn't build one.
- Implementation itself — like every ADR before it, this is a decision record only.
