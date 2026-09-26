# ADR-0056: Cloudflare account/zone ID secrets

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0004.

## Context

`root.hcl` already has an established pattern for keeping account-identifying values out of tracked files: `local.secrets` generically parses every `KEY=value` line from the git-ignored root `.env` (no per-key parsing code needed — any new key just works), and values are threaded into generated (also git-ignored, `.terragrunt-cache`-only) `.tf` files via `get_env("KEY", lookup(local.secrets, "KEY", ""))`. This is how `CLOUDFLARE_TOKEN` and the DigitalOcean credentials already reach the `provider` blocks, and how ADR-0054 added `pigeon_tf_root` as an injected local available to every leaf.

`topology-v1` did **not** follow this pattern for Cloudflare zone/account IDs — `pigeon.dev/env.tf` hardcoded them as plain literals directly in a tracked, committed file:

```hcl
locals {
  cloudflare_account_id = "<redacted>"
  cloudflare_zone_id    = "<redacted>"
}
```

Harmless in a private repo; it would leak real account-identifying values if copied as-is into `pigeon-do`, which is public. This ADR defines `CLOUDFLARE_PIGEON_DEV_ZONE_ID` and `CLOUDFLARE_ACCOUNT_ID` in `.env` instead, injected into Terraform so a `data` source can consume them, with the raw values never appearing in any tracked file.

Two ways to expose the injected values were considered:

- **Locals available to any leaf** (matches `pigeon_tf_root`'s precedent): `root.hcl` generates `local.cloudflare_account_id`/`local.cloudflare_pigeon_dev_zone_id`; each leaf that needs them writes its own ordinary `data`/resource blocks referencing the local. **Chosen** — leaves that have nothing to do with Cloudflare (pure DigitalOcean leaves) pay no cost.
- **`root.hcl` centrally generates the `data "cloudflare_zone"` block itself** (like the `provider` block does with credentials): every leaf would get `data.cloudflare_zone.pigeon_dev` for free, but this would force *every* leaf — including DO-only ones — to resolve a live Cloudflare API call on every plan/apply, and would require a valid `CLOUDFLARE_TOKEN` even where nothing Cloudflare-related is happening. Rejected for that coupling/performance cost.

## Decision

### New `.env`/`.env.example` keys

- `CLOUDFLARE_ACCOUNT_ID` — account-wide, unprefixed, since a Cloudflare account isn't per-domain.
- `CLOUDFLARE_PIGEON_DEV_ZONE_ID` — per-domain (`CLOUDFLARE_<DOMAIN>_ZONE_ID`), since zones are per-domain. Future domains add their own `CLOUDFLARE_<DOMAIN>_ZONE_ID` key without touching the account key.
- No `.env`-parsing changes needed in `root.hcl` — `local.secrets` already picks up any key generically.

### `root.hcl` injection

A new `generate "cloudflare_ids"` block, parallel to the existing `generate "pigeon_tf"` block:

```hcl
generate "cloudflare_ids" {
  path      = "cloudflare_ids_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
locals {
  cloudflare_account_id         = "${get_env("CLOUDFLARE_ACCOUNT_ID", lookup(local.secrets, "CLOUDFLARE_ACCOUNT_ID", ""))}"
  cloudflare_pigeon_dev_zone_id = "${get_env("CLOUDFLARE_PIGEON_DEV_ZONE_ID", lookup(local.secrets, "CLOUDFLARE_PIGEON_DEV_ZONE_ID", ""))}"
}
EOF
}
```

Same shell-env-wins-over-`.env` precedence already used for provider credentials.

### Zone data-source leaf

`pigeon.dev/cloudflare/global/zone/` — a standard leaf (`terragrunt.hcl` includes root, `terraform { source = get_terragrunt_dir() }`) plus a `.tf` file shaped like:

```hcl
data "cloudflare_zone" "pigeon_dev" {
  zone_id = local.cloudflare_pigeon_dev_zone_id
}
```

Exact argument names/shape will be confirmed against the pinned `cloudflare/cloudflare ~> 5` provider's actual schema at implementation time — not independently verified while writing this ADR. The architecturally-decided part is that the raw ID only ever flows through the injected local, never a literal in a tracked file. Other leaves that need `local.cloudflare_account_id` (e.g. future Cloudflare resources requiring an explicit account ID) reference the same injected local directly — no separate mechanism needed.

## Consequences

- Any future Cloudflare-related leaf can reference `local.cloudflare_account_id`/`local.cloudflare_pigeon_dev_zone_id` with zero additional wiring.
- The raw values never appear in a tracked file.
- Leaves unrelated to Cloudflare are unaffected — no new data source cost imposed on them.

## Out of scope

- Additional domains/zones — each would get its own `CLOUDFLARE_<DOMAIN>_ZONE_ID` key later, following the same convention.
- Actual DNS records or other Cloudflare resources.
- Any generalized multi-zone abstraction.
