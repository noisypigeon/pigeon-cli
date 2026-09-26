# ADR-0063: shared root .env, two Cloudflare accounts, Cloudflare state → Scaleway backend

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0011.

## Context

Four things are wrong/incomplete in the current secrets and Cloudflare setup:

1. **ADR-0061's "shared `.env`" design was never actually built.** It describes `cloudflare/root.hcl`/`scaleway/root.hcl` reading `digitalocean/.env` via an explicit `"${get_repo_root()}/digitalocean/.env"` path. What's actually on disk: all three (`digitalocean/root.hcl`, `cloudflare/root.hcl`, `scaleway/root.hcl`) independently run `find_in_parent_folders(".env", "")`, each resolving to **its own** directory's `.env` — four separate files exist today (`digitalocean/.env`, `scaleway/.env`, `cloudflare/.env`, plus `pigeon.dev.env`, which doesn't even exist on disk — only its `.example` does). They've drifted: `cloudflare/.env` is missing the `DIGITALOCEAN_SPACES_*`/`DIGITALOCEAN_TERRAFORM_STATE_BUCKET` keys its own backend currently requires, and instead has an unrelated full copy of Scaleway credentials.

2. **Cloudflare is treated as one account when it's actually two.** ADR-0056 explicitly assumed a single Cloudflare account ("account-wide, unprefixed, since a Cloudflare account isn't per-domain"). Today `CLOUDFLARE_TOKEN`/`CLOUDFLARE_ACCOUNT_ID` are unsuffixed and reused **identically** by both `cloudflare/root.hcl` (`noisypigeon.com`'s three leaves) and `pigeon.dev.hcl` (`pigeon.dev`'s two leaves) — there's no way to represent two accounts with these var names today. Since `noisypigeon.com` and `pigeon.dev` are different Cloudflare accounts, one domain's leaves are presumably being pointed at the wrong account's token right now (or `pigeon.dev`'s leaves simply have never had working credentials, since `pigeon.dev.env` doesn't exist on disk).

3. **All 5 Cloudflare leaves still use the shared DigitalOcean Spaces backend** (`cloudflare/root.hcl` and `pigeon.dev.hcl` both point `remote_state` at `tor1.digitaloceanspaces.com`), per ADR-0061. ADR-0062 already proved out a dedicated Scaleway Object Storage bucket (`management-diw0s1-terraform-state`, `https://s3.fr-par.scw.cloud`) for `scaleway/*` — Cloudflare state can move onto the same bucket rather than staying on the DigitalOcean one.

4. `pigeon.dev.hcl` is a structural near-duplicate of `cloudflare/root.hcl` (same secrets-loading shape, same `generate` block shapes, same `remote_state` shape) maintained by hand as a separate file — this ADR doesn't merge them (ADR-0061's reasoning for keeping them separate, different per-domain zone IDs, still holds), but does bring their secrets source and backend into alignment.

## Decision

### One root-level `.env` / `.env.example`

Add `/.env` and `/.env.example` at the true repo root, combining DigitalOcean, Scaleway, and Cloudflare (both accounts). Because `find_in_parent_folders(".env", "")` is already what `digitalocean/root.hcl`, `cloudflare/root.hcl`, and `scaleway/root.hcl` each do today — just resolving to their own directory first — **deleting the three per-provider `.env` files requires zero code change to that lookup logic**: each one naturally walks up and finds the new repo-root `.env` instead. `pigeon.dev.hcl` currently uses an explicit `"${get_repo_root()}/pigeon.dev.env"` path; change it to the same `find_in_parent_folders(".env", "")` pattern as the other three, for consistency. Delete `digitalocean/.env(.example)`, `scaleway/.env(.example)`, `cloudflare/.env(.example)`, and `pigeon.dev.env.example`.

New root `.env.example`:
```
# Copy this file to .env (repo root) and fill in real values. That file is
# git-ignored and must never be committed — see docs/adr/0063-shared-root-env-and-cloudflare-migration.md.
# This replaces the old digitalocean/.env, scaleway/.env, cloudflare/.env,
# and pigeon.dev.env — every provider's root.hcl (and pigeon.dev.hcl) reads
# this one file via find_in_parent_folders(".env", "").

# DigitalOcean
DIGITALOCEAN_TOKEN=
DIGITALOCEAN_SPACES_ACCESS_ID=
DIGITALOCEAN_SPACES_SECRET_KEY=
DIGITALOCEAN_TERRAFORM_STATE_BUCKET=
# Bucket Names
BUCKET_NAME_DATA_EMAIL=
BUCKET_NAME_BACKBLAZE_IMPORT=

# Scaleway
SCALEWAY_ACCESS_KEY=
SCALEWAY_SECRET_KEY=
SCALEWAY_ORGANIZATION_ID=
SCALEWAY_PROJECT_ID_NOISYPIGEON_COM=
SCALEWAY_PROJECT_ID_PIGEON_DEV=
# Also the Terraform state backend for scaleway/* and cloudflare/*.
SCALEWAY_TERRAFORM_STATE_BUCKET_NAME=

# Cloudflare — noisypigeon.com and pigeon.dev are different Cloudflare
# accounts, so each domain gets its own token/account id.
CLOUDFLARE_NOISYPIGEON_COM_TOKEN=
CLOUDFLARE_NOISYPIGEON_COM_ACCOUNT_ID=
CLOUDFLARE_NOISYPIGEON_COM_ZONE_ID=
CLOUDFLARE_PIGEON_DEV_TOKEN=
CLOUDFLARE_PIGEON_DEV_ACCOUNT_ID=
CLOUDFLARE_PIGEON_DEV_ZONE_ID=
```

### Split Cloudflare vars into two accounts

Rename `CLOUDFLARE_TOKEN`/`CLOUDFLARE_ACCOUNT_ID` to domain-suffixed vars — `CLOUDFLARE_NOISYPIGEON_COM_TOKEN`/`CLOUDFLARE_NOISYPIGEON_COM_ACCOUNT_ID` (used only by `cloudflare/root.hcl`'s `generate` blocks) and `CLOUDFLARE_PIGEON_DEV_TOKEN`/`CLOUDFLARE_PIGEON_DEV_ACCOUNT_ID` (used only by `pigeon.dev.hcl`'s) — matching the naming precedent already set by `CLOUDFLARE_NOISYPIGEON_COM_ZONE_ID`/`CLOUDFLARE_PIGEON_DEV_ZONE_ID` and by `SCALEWAY_PROJECT_ID_NOISYPIGEON_COM`/`SCALEWAY_PROJECT_ID_PIGEON_DEV`. No provider aliasing needed: each domain's leaves already resolve through a separate root/domain `.hcl` file (ADR-0061), so each file's single `provider "cloudflare"` block just points at its own domain's vars.

### Migrate Cloudflare state to the Scaleway backend

Both `cloudflare/root.hcl`'s and `pigeon.dev.hcl`'s `remote_state` blocks move off the shared DigitalOcean Spaces bucket onto the same Scaleway bucket ADR-0062 set up (`management-diw0s1-terraform-state`, `https://s3.fr-par.scw.cloud`, region `fr-par`, `endpoints.s3` form), reusing `SCALEWAY_ACCESS_KEY`/`SCALEWAY_SECRET_KEY` for backend auth, same as `scaleway/root.hcl`. `cloudflare/root.hcl` keeps its existing `"cloudflare/${path_relative_to_include()}/terraform.tfstate"` key prefix unchanged; `pigeon.dev.hcl` keeps its existing unprefixed `"${path_relative_to_include()}/terraform.tfstate"` key unchanged (already naturally includes `cloudflare/...` since it's a repo-root file). Once this lands, neither file needs any `DIGITALOCEAN_*` var at all — only `digitalocean/root.hcl`'s own backend still does.

**Migration runbook** (documented, not executed by this ADR): all 5 Cloudflare leaves are already on a remote (S3) backend today, not local — so migrating means changing backend config, then running `terragrunt init -migrate-state -force-copy` per leaf to copy each leaf's state object from the DigitalOcean bucket to the Scaleway bucket — same mechanics as ADR-0062's per-leaf migrations, but bucket-to-bucket rather than local-to-remote.

## Consequences

- One root `.env` instead of four scattered ones; `digitalocean/.env`, `scaleway/.env`, `cloudflare/.env`, and both `.env.example` counterparts plus `pigeon.dev.env.example` are all deleted.
- `CLOUDFLARE_TOKEN`/`CLOUDFLARE_ACCOUNT_ID` no longer exist as var names — anything currently relying on them (shell exports, CI secrets) needs updating to the new domain-suffixed names.
- `cloudflare/root.hcl` and `pigeon.dev.hcl` drop `DIGITALOCEAN_SPACES_*`/`DIGITALOCEAN_TERRAFORM_STATE_BUCKET` usage entirely.
- All 5 Cloudflare leaves' state **keys** are unchanged — only the bucket/endpoint/credentials move, so this is a backend swap requiring a one-time state copy, not a path rename.
- The Scaleway `terraform-deployer` IAM key becomes the backend credential for `scaleway/*` **and** `cloudflare/*` state — its blast radius grows again (already flagged as load-bearing in ADR-0062).
- One shared root `.env` is a single point of failure/leak instead of four, but also a single place to rotate/audit instead of four drift-prone copies.

## Out of scope

- Renaming `CLOUDFLARE_TOKEN`/`CLOUDFLARE_ACCOUNT_ID` in any CI/CD secret store outside this repo.
- Auditing which Cloudflare account each domain's zone actually belongs to today, or confirming whether `noisypigeon.com`/`pigeon.dev` leaves have in fact been applying against the wrong account — flagged as a real risk this change surfaces, not resolved by it.
- Running the per-leaf migration runbook (`terragrunt init -migrate-state`) for any of the 5 Cloudflare leaves — documented here, executed as a separate follow-up (same pattern as ADR-0062).
- Narrowing the Scaleway deployer key's permission scope now that it backs two providers' state — already deferred in ADR-0062, still deferred here.
- ~~Merging `pigeon.dev.hcl` into `cloudflare/root.hcl` — they stay separate per-domain files per ADR-0061's reasoning (different zone IDs, and now different accounts too).~~ Superseded by ADR-0064, which merges them via path-based credential branching.
