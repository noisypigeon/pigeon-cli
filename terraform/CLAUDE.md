# CLAUDE.md

`pigeon-tf` is a versioned, reusable Terraform modules repo (DigitalOcean, Scaleway), consumed locally by `pigeon-do` and future infra repos — no root provider/backend config, never run standalone.

## ADRs govern this project

Before making architectural or interface changes, read the ADRs in `docs/adr/` and keep new work consistent with their decisions. If a change would contradict an existing ADR, flag it rather than silently diverging — prefer writing a new ADR (or updating an existing one's Status) over undocumented drift.

- `docs/adr/0001-pigeon-tf-scaffold.md` — repo layout, module self-containment (per-module `versions.tf`), versioning/tagging, and local-clone consumption model.
- `docs/adr/0006-add-scaleway-provider.md` — adds the second provider root (`scaleway/`), generalizes `module-docs.yml`/`module-release.yml` beyond a single hardcoded provider root, and fixes a latent per-module tag-naming bug.
