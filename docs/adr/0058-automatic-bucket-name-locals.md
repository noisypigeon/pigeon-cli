# ADR-0058: automatic bucket-name locals from .env

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0006.

## Context

`root.hcl` has a hand-maintained `generate "bucket_names"` block, following the exact pattern of the existing `cloudflare_ids`/`pigeon_tf` blocks:

```hcl
generate "bucket_names" {
  path      = "bucket_names_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
locals {
  rolodex_email_bucket_name         = "${get_env("ROLODEX_EMAIL_BUCKET_NAME", lookup(local.secrets, "ROLODEX_EMAIL_BUCKET_NAME", ""))}"
  rolodex_poutine_2021_bucket_name  = "${get_env("ROLODEX_POUTINE_2021_BUCKET_NAME", lookup(local.secrets, "ROLODEX_POUTINE_2021_BUCKET_NAME", ""))}"
}
EOF
}
```

Every new bucket needs a new hand-written line here — exactly the "edit a central file for every new item" friction `pigeon_tf_root` (ADR-0054) and the per-module named-local pattern it replaced were designed to avoid. Adding a bucket should only ever require editing `.env`/`.env.example`.

Key enabling fact: `local.secrets` (established in ADR-0053) generically parses **every** `KEY=value` line from `.env` into a map — nothing about it is specific to the currently-known keys. The current block's real limitation isn't `.env` parsing, it's that each bucket-name local is individually spelled out by name in the `generate` block's `contents`, rather than derived by iterating the map.

Keys are grouped by a `BUCKET_NAME_` prefix (matching the categorical-prefix style already used for `CLOUDFLARE_*`/`DIGITALOCEAN_*` keys), but the Terraform locals they produce keep the existing `*_bucket_name` suffix naming (`rolodex_email_bucket_name`, matching what every leaf already references) — so the key and the local it produces are shaped differently on purpose, and the mechanism has to actually reorder the name, not just lowercase it. This ADR scopes the automatic mechanism to that `BUCKET_NAME_` prefix specifically — not "expose every unclaimed `.env` key as a local" (broader, and would risk exposing something like `CLOUDFLARE_TOKEN` itself as a plain local if not carefully excluded). `BUCKET_NAME_` is a safe, narrow, purpose-built filter matching exactly what's needed.

**Preserving the existing precedence**: every other secret in `root.hcl` uses `get_env("KEY", lookup(local.secrets, "KEY", ""))` so a real shell env var can override the `.env` file value. `get_env()`'s first argument doesn't need to be a literal — it accepts any string-valued expression, so a `for` expression can call `get_env(k, v)` with a dynamically-iterated key and still get that same override behavior automatically, for every matched key, with no per-key code. This specific behavior — a computed, non-literal argument to `get_env()` — will be confirmed empirically during implementation, rather than asserted untested here.

## Decision

### Discovery convention

Any `.env`/`.env.example` key prefixed with `BUCKET_NAME_` is automatically exposed as a Terraform local — but the prefix is stripped, not preserved: the remainder is lowercased and given the existing `_bucket_name` suffix instead (`BUCKET_NAME_ROLODEX_EMAIL` → `local.rolodex_email_bucket_name`). This is a real reordering, not a case-fold — it exists specifically so the local name matches what every leaf already references, even though the `.env` key itself is shaped differently (prefix-grouped, like `CLOUDFLARE_*`/`DIGITALOCEAN_*`).

### `root.hcl` mechanism

A new local, computed alongside the existing `secrets` local:

```hcl
bucket_name_secrets = {
  for k, v in local.secrets : "${lower(trimprefix(k, "BUCKET_NAME_"))}_bucket_name" => get_env(k, v)
  if startswith(k, "BUCKET_NAME_")
}
```

`trimprefix()` is the same standard Terraform/Terragrunt stdlib function family as the `trimspace()`/`startswith()` already used elsewhere in `root.hcl`. Walked through: `trimprefix("BUCKET_NAME_ROLODEX_EMAIL", "BUCKET_NAME_")` → `"ROLODEX_EMAIL"` → `lower()` → `"rolodex_email"` → `"rolodex_email_bucket_name"`.

The `generate "bucket_names"` block's `contents` is rebuilt from that map instead of hand-listing keys:

```hcl
generate "bucket_names" {
  path      = "bucket_names_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
locals {
${join("\n", [for k, v in local.bucket_name_secrets : "  ${k} = \"${v}\""])}
}
EOF
}
```

### Adding a bucket going forward

Add `BUCKET_NAME_NEW_THING=` to `.env` and `.env.example` → available as `local.new_thing_bucket_name`. Nothing in `root.hcl` changes, ever, for this category.

### Renaming the two existing keys

`.env`/`.env.example` currently have `ROLODEX_EMAIL_BUCKET_NAME` and `ROLODEX_POUTINE_2021_BUCKET_NAME` — the old suffix shape, predating this ADR. Implementing this ADR includes renaming both to the prefix form, `BUCKET_NAME_ROLODEX_EMAIL` and `BUCKET_NAME_ROLODEX_POUTINE_2021`. The locals they produce (`rolodex_email_bucket_name`, `rolodex_poutine_2021_bucket_name`) are unchanged, so no leaf needs updating — only the `.env`/`.env.example` key spellings change.

### Empty-set behavior

If no `.env` key matches the prefix, the generated file is a valid (empty) `locals {}` block — no error.

## Consequences

- `root.hcl` no longer needs editing to add or remove a bucket-name secret.
- The shell-env-var-override precedence is preserved automatically for every matched key.
- The `BUCKET_NAME_` naming convention becomes load-bearing — a typo'd prefix silently means "not picked up," rather than a hard error.
- The two existing `.env`/`.env.example` keys need a one-time rename to adopt the prefix form.

## Out of scope

- Generalizing this beyond the `BUCKET_NAME_` prefix to any other category of `.env` key (e.g. project names, region overrides) — a separate decision if/when that friction shows up for something else.
- Any validation/enforcement that a referenced `local.<x>_bucket_name` actually has a matching `.env` key — a leaf referencing a local that doesn't exist yet just fails at plan time with a normal "undefined local" error, same as today.
