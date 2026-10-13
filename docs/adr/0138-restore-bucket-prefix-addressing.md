# ADR-0138: restore bucket-plus-prefix addressing (`alias:prefix/`)

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

ADR-0136 replaced `transform`'s and `rclone copy`/`delete`'s raw
`remote:path` strings with pigeon keyring `BucketConfig` aliases, and in
doing so collapsed every address to a whole bucket root:

```rust
// src/core/rclone/config.rs, as ADR-0136 left it
pub(crate) fn remote_address(alias: &str) -> String { format!("{alias}:") }
```

ADR-0136 §2 stated this as "No sub-path affordance", and its Out of scope
section deferred the capability: *"A sub-path addressing mode for a
bucket-config alias (e.g. `alias:subfolder/`) -- every alias addresses its
whole bucket root only... A future ADR can add this if a real need for it
surfaces."*

That need is now confirmed, and more pointedly, removing it was a
**regression rather than a deferral**. The real deployment addresses one
`source` bucket and one `destination` bucket, each holding per-extension
*directories* -- `source:mp4/` transcoded into `destination:mp4/` -- so
working invocations of the form

```
job run transform --input-file-type=mp4 \
  --source-path 'source:mp4/' --destination-path 'destination:mp4/' ...
```

became unexpressible. "One whole bucket per file type" is not the layout
in use, and nothing in ADR-0136 verified that it was before removing the
ability to say otherwise.

ADR-0137, merged the same day, compounded this rather than catching it: it
inferred the bucket *alias* from `--input-file-type` (`--input-file-type
mp4` → a bucket-config aliased `mp4`), a convention this deployment does
not use at all. It also carried a latent collision -- `InputFileType::
output_extension()` returns `"mp4"` for every video kind, so a
`mov`/`m4v`/`mp4` run with both flags omitted would have resolved source
and destination to the *same* alias.

## Decision

### 1. `alias:prefix/` composition in `core::rclone::config`

The bare `remote_address(alias)` is replaced by a small parsed type. The
old spelling is reused exactly, because it is already valid `rclone`
syntax against the config pigeon generates -- `write_sandboxed_config`
keys each section by alias, so `source:mp4/` resolves natively with no
new translation layer:

```rust
pub(crate) struct RemoteTarget { alias: String, prefix: Option<String> }

/// Splits on the FIRST ':' -- `source:mp4/` -> alias "source", prefix
/// "mp4/". A bare `source` (or `source:`) is the whole bucket root,
/// identical to ADR-0136's behavior. The alias half is validated against
/// `Store::contains_alias` exactly as ADR-0136/ADR-0137 already did.
fn parse(value: &str, store: &Store) -> Result<RemoteTarget, String>

/// `alias:` or `alias:prefix/` -- the prefix is normalized to always end
/// in '/' when non-empty, so `deduplicate`'s existing
/// `format!("{}{key}", ...)` concatenation stays correct by construction
/// rather than by each call site remembering to add a separator.
fn address(&self) -> String
```

`parse` strips a leading `/`, appends a trailing `/` to a non-empty
prefix, treats an empty prefix as the bucket root, and **rejects any `..`
path segment** -- `rclone` would take it literally, so refusing is
clearer than silently escaping the prefix the operator asked for.
Splitting on the first colon means an alias containing `:` is
unaddressable in this form; stated as a consequence rather than guarded,
since no such alias exists and `keyring add`'s alias prompt is free-form.

### 2. Scope: `transform` and `rclone copy`/`delete`

These are exactly the three job variants ADR-0136 regressed -- each held
raw `remote:path` values under ADR-0101/ADR-0112 before it.
`transform`'s `--source`/`--destination` and `rclone copy`/`delete`'s
`--source-bucket`/`--destination-bucket` all accept `alias[:prefix/]`.
No flag is renamed here: ADR-0137's `--source`/`--destination` rename
stands.

`deduplicate` is deliberately **out of scope**. It never had sub-path
addressing (its whole-bucket model predates the raw-path era -- ADR-0136
§4 kept it unchanged for that reason), and its checkpoint identity is
keyed on the bucket alias: the `.processed` checkpoint, `ContentIndex`'s
4th field, two report columns, and four in-memory compound keys
(ADR-0109). Two runs sharing an alias but differing by prefix would
collide in that keyspace, which is checkpoint-format work this change
does not require. Filed as an issue per ADR-0031 rather than silently
skipped.

### 3. ADR-0137's inference moves from the alias to the prefix

ADR-0137's intent -- don't retype what `--input-file-type` already knows
-- is kept, but relocated to the half of the address that actually varies
per file type:

- `--source source` with `--input-file-type mp4` → `source:mp4/`
  (prefix defaults to `input_file_type.extension()`)
- `--destination destination` → `destination:mp4/`
  (prefix defaults to `input_file_type.output_extension()`)
- An explicit prefix always wins verbatim: `--source 'source:raw-video/'`
  stays `source:raw-video/`, with no defaulting applied.

The **alias half is no longer inferred at all**, reversing ADR-0137's
central mechanism. `--source`/`--destination` are mandatory again on a
non-interactive run; interactively they are still picked via
`Store::prompt_select_bucket`. This also removes the video alias
collision described in Context, since source and destination now differ
by prefix under operator-named buckets instead of racing for one
extension-named alias.

Consequently ADR-0137's `default_alias: Option<&str>` parameter on
`prompt_select_bucket` reaches zero meaningful callers -- all 7 sites
would pass `None` -- so that parameter and its extracted
`default_bucket_index` helper are **deleted**, returning
`src/commands/keyring/store.rs` to its pre-ADR-0137 signature.
`clippy -D warnings` forces this, the same mechanical zero-consumer proof
ADR-0094/ADR-0131 used when un-hoisting. The 2-or-more-bucket-config
`Select`-branch smoke test ADR-0137 added is kept (still the only
coverage that branch has ever had), now hint-free.

### 4. No prefix reaches a metric label

A prefix is operator-chosen and therefore unbounded cardinality, so it
must never become a label value (ADR-0102/ADR-0130's "no raw paths as
metric labels" rule). Verified rather than assumed: `transform`'s
`Copy`/`CopyTo` invocations label only `job`/`step` and carry no bucket
label at all, and every job that *does* carry a `source_bucket`/
`destination_bucket` label takes it from the resolved `BucketConfig`'s
alias, which this change does not touch. `RemoteTarget` deliberately
exposes no alias accessor -- one was written, had zero consumers for
exactly this reason, and was deleted under `clippy -D warnings`. No label
value changes anywhere, so dashboards are unaffected.

## Consequences

- **`transform`'s destination filenames change for a prefix-addressed
  run.** `placement::compute_destination_name` hashes `source_path +
  "\u{1}" + relative_path`; `source_path` goes from `source:` to
  `source:mp4/`, so every output name differs from a pre-change run
  against the same bucket. A rerun wants a fresh `--local-output`, and
  ADR-0124's skip-already-done listing will not match prior output.
  Acceptable -- a prefix-addressed run is a genuinely different source
  address -- but it is not a silent no-op and is named here rather than
  discovered later.
- **ADR-0137 ships and is then largely reversed within one day.**
  Recorded plainly: its `--source`/`--destination` rename survives, its
  alias-inference mechanism does not. The honest reason is that ADR-0137
  was written against an assumed bucket-naming convention that the real
  deployment does not use -- the same unverified assumption ADR-0136 made
  when it removed sub-paths in the first place.
- `--report-bucket` stays alias-only on every job (it is a `minio` leg,
  not an rclone one) -- untouched, as in ADR-0136.
- A prefix is passed to `rclone` as given (after normalization); pigeon
  does not verify that the prefix exists in the bucket. An empty or
  typo'd prefix yields an empty listing rather than an error, the same
  behavior issue [#63](https://github.com/noisypigeon/pigeon-cli/issues/63)
  already tracks for `Lsjson`'s "directory not found" tolerance.

## Out of scope

- Sub-path addressing for `deduplicate`'s `--source-bucket`/ ([#73](https://github.com/noisypigeon/pigeon-cli/issues/73))
  `--destination-bucket` -- its alias-keyed checkpoint identity (ADR-0109)
  would need rework first; filed per ADR-0031.
- Any `BucketConfig` schema change to store a default prefix per bucket --
  the prefix is a per-run addressing concern, not bucket metadata.
- Validating that a prefix actually exists in the bucket before the run
  (see Consequences).
- Reintroducing arbitrary non-keyring `remote:path` passthrough --
  ADR-0136's sandboxing of `rclone` to pigeon-generated config stands
  entirely; only the sub-path half of its addressing decision is
  reversed.

## Amends

ADR-0136 (reverses its "no sub-path affordance" decision and resolves its
own deferred Out of scope item; its sandboxed-config and
alias-validation decisions are untouched) and ADR-0137 (reverses its
bucket-alias inference and the `prompt_select_bucket` default-alias hint
it added, replacing both with prefix inference; keeps its
`--source`/`--destination` rename). Each gets a short forward-pointing
"Amendment" section, per this repo's convention for amending a merged ADR
without rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `cargo clippy --all-targets --all-features -- -D warnings` clean
  specifically after `default_bucket_index`'s deletion -- the mechanical
  proof the zero-consumer census was complete.
- New `RemoteTarget::parse` unit tests: bare alias, `alias:`,
  `alias:prefix`, `alias:prefix/`, a leading `/` stripped, an unknown
  alias erroring by name, and a `..` segment rejected.
- `transform::wizard` unit tests: an omitted prefix defaults to the
  input/output extension, an explicit prefix wins verbatim, and an
  omitted `--source`/`--destination` errors non-interactively again.
- `tests/cli.rs`: `transform --help` documents the `alias:prefix/` form;
  a non-interactive run omitting `--source` fails fast; an explicit
  `alias:prefix/` resolves; an unknown alias in the composed form still
  errors by alias name.
- Manual, the actual regression case: `job run transform
  --input-file-type=mp4 --source 'source:mp4/' --destination
  'destination:mp4/' --report-bucket reports --non-interactive ...`, plus
  the prefix-defaulted short form (`--source source --destination
  destination`) resolving to those same two addresses.
