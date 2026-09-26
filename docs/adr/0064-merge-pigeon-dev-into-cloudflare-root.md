# ADR-0064: merge pigeon.dev.hcl into cloudflare/root.hcl

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0012.

## Context

ADR-0061 kept `pigeon.dev.hcl` as a separate, hand-maintained file from `cloudflare/root.hcl` because "a shared `cloudflare/root.hcl` can't be domain-specific enough for both" — at the time, the only per-domain difference was the zone ID. ADR-0063 preserved that separation explicitly (its "Out of scope" lists "Merging `pigeon.dev.hcl` into `cloudflare/root.hcl` — they stay separate per-domain files per ADR-0061's reasoning"), while also establishing that `noisypigeon.com` and `pigeon.dev` are different Cloudflare accounts and introducing domain-suffixed credential vars (`CLOUDFLARE_NOISYPIGEON_COM_*`/`CLOUDFLARE_PIGEON_DEV_*`) to represent that.

With those domain-suffixed vars now in place, the two files are near-byte-identical duplicates differing only in which suffixed vars they read. Checked both domains' leaf `.tf` files directly:

- `zone.tf` in both `noisypigeon.com/zone` and `pigeon.dev/zone` references the **same unsuffixed** `local.cloudflare_account_id` — this needs path-based branching to resolve to the correct domain's account.
- `fastmail`/`github` leaves reference **domain-suffixed** zone-id locals (`local.cloudflare_noisypigeon_com_zone_id` vs `local.cloudflare_pigeon_dev_zone_id`) — no collision, both can simply always be generated side by side.
- The provider block's `api_token` needs the same path-based branching as `account_id`, since each leaf only ever needs one domain's token, never both at once.

State-key continuity is verified: `cloudflare/root.hcl`'s existing key, `"cloudflare/${path_relative_to_include()}/terraform.tfstate"`, evaluated for a `pigeon.dev` leaf (e.g. `cloudflare/global/pigeon.dev/fastmail`) produces `cloudflare/global/pigeon.dev/fastmail/terraform.tfstate` — identical to what `pigeon.dev.hcl`'s own key (`"${path_relative_to_include()}/terraform.tfstate"`, evaluated from the repo root) already produces for that same leaf. This merge is a backend-location and include-mechanism consolidation, not a state-key rename.

## Decision

`cloudflare/root.hcl` computes a single local, `is_pigeon_dev_leaf = startswith(path_relative_to_include(), "global/pigeon.dev/")`, and uses it to branch:

- `cloudflare_account_id` (the `generate "cloudflare_ids"` block) — `CLOUDFLARE_PIGEON_DEV_ACCOUNT_ID` when true, `CLOUDFLARE_NOISYPIGEON_COM_ACCOUNT_ID` otherwise.
- The provider's `api_token` (the `generate "provider"` block) — same branching, `CLOUDFLARE_PIGEON_DEV_TOKEN`/`CLOUDFLARE_NOISYPIGEON_COM_TOKEN`.

`cloudflare_noisypigeon_com_zone_id` and `cloudflare_pigeon_dev_zone_id` are both always generated, unconditionally — no branching needed since leaf `.tf` files already reference their own domain-specific name and there's no naming collision.

Both `pigeon.dev` leaves (`cloudflare/global/pigeon.dev/fastmail`, `cloudflare/global/pigeon.dev/zone`) switch their `include "domain" { path = find_in_parent_folders("pigeon.dev.hcl") }` block to `include "root" { path = find_in_parent_folders("root.hcl") }`, identical to how every `noisypigeon.com` leaf already includes `cloudflare/root.hcl`. `pigeon.dev.hcl` is deleted. No leaf `.tf` file changes — only the generated locals they already reference change source.

## Consequences

- One `cloudflare/root.hcl` manages all 5 Cloudflare leaves (3 `noisypigeon.com` + 2 `pigeon.dev`) instead of two hand-maintained near-duplicate files.
- A leaf's credentials are now implicitly determined by its directory path (`global/pigeon.dev/*` vs everything else under `cloudflare/`) rather than by which file it explicitly includes. A leaf accidentally placed outside the expected `global/pigeon.dev/` or `global/<other-domain>/` path structure would silently resolve to `noisypigeon.com`'s credentials — there's no code-level guard against this, only the existing directory convention.
- Supersedes ADR-0063's "Out of scope" bullet on this exact topic.

## Out of scope

- Any change to leaf `.tf` files — none are needed.
- The migration/import execution itself (moving `pigeon.dev`'s leaves off the DigitalOcean backend, importing real DNS records) — documented and executed as a separate implementation task, not part of this ADR.
- Provider aliasing as an alternative mechanism — not used, since each leaf only ever needs one domain's credentials at a time; path-based branching is simpler and requires no leaf-level `provider = cloudflare.x` meta-arguments.
