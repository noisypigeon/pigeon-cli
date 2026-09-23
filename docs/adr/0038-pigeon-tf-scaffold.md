# ADR-0001: pigeon-tf repo scaffold

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

`pigeon-tf` exists as a repo separate from `pigeon-do` because `topology-v1`'s in-repo `modules/` tree wasn't actually reusable on its own: those modules relied on the consuming repo's root config to inject the right Terraform providers, so lifting one out (or referencing it from a second repo) meant silently depending on config it didn't own. `pigeon-do`'s [ADR-0002](https://github.com/noisypigeon/pigeon-do/blob/main/docs/adr/0002-pigeon-tf-scaffold.md) made the actual scaffold decisions for this repo — layout, module self-containment, versioning, and consumption model — from the consumer's side, since `pigeon-tf` didn't exist yet at the time.

This ADR backfills that decision into `pigeon-tf` itself, now that it's history (`v0.1.0` through `v0.1.3`), rather than duplicating `pigeon-do`'s ADR-0002 prose. It also introduces `docs/adr/` and `CLAUDE.md` to this repo for the first time — both `pigeon-do` and `pigeon-cli` have had these since their own bootstrap ADRs, and `pigeon-tf` has grown past its initial scaffold without either.

## Decision

### Repo layout

- Root holds provider/resource directories directly — `digitalocean/access-key`, `digitalocean/object-bucket`, `digitalocean/object-bucket-cold`, `digitalocean/project` — with no wrapping `modules/` directory, since the whole repo already is a module collection.
- No `LICENSE` file, matching the existing convention across `pigeon-cli` and `topology-v1`.
- No root provider/backend configuration — this repo is never `terraform`/`terragrunt` run standalone, only consumed as module sources by other repos.
- `README.md` is the module index.

### Module self-containment

- Every module owns its own `versions.tf` declaring exactly the providers it uses (`digitalocean/digitalocean ~> 2.0` on all of them; `hashicorp/random ~> 3.0` only where actually used, e.g. `object-bucket`'s `random_string` suffix).
- This closes the gap `topology-v1` had: a module consumed from a different root config no longer silently depends on that repo's `root.hcl` happening to inject the right provider.

### Versioning

- Semantic-version git tags (`vX.Y.Z`) on `main`. No CI or release automation — a GitHub Release is cut by hand per tag.
- Three modules (`access-key`, `object-bucket`, `project`) were ported and released at `v0.1.0`. `object-bucket-cold` was added at `v0.1.2` — a data-source-only wrapper, since Cold Storage buckets aren't yet supported as a Terraform resource by the DigitalOcean provider.
- `README.md`'s module table is the living, authoritative list of what's available — new modules land via an ordinary PR and version bump, not a new ADR, unless they change the scaffold conventions decided here.

### Consumption

- Consumers clone this repo as a sibling directory and `git checkout` the tag they want — no submodule, no sync script, no lockfile.
- Deliberate simplicity-over-enforcement tradeoff: version drift between what's tagged and what's checked out locally is possible and won't be caught automatically. Acceptable while Terragrunt is only ever run locally; revisit if that stops being true.

### Documentation conventions (new as of this ADR)

- Adds `docs/adr/` and `CLAUDE.md`, mirroring `pigeon-do`/`pigeon-cli`, so future changes to this repo get the same "ADRs govern this project" discipline those repos already have.

## Consequences

- Contributors (and Claude Code) now have one place in `pigeon-tf` itself documenting why the repo is shaped the way it is, without needing to cross-reference `pigeon-do`.
- Future structural changes to `pigeon-tf` (new provider, module layout change, versioning change) should get their own ADR here, not only a note in the consuming repo.

## Out of scope

- CI or release automation.
- A `LICENSE` file.
- Per-module ADRs for routine module additions — only scaffold-level changes warrant a new ADR.
- Any change to `pigeon-do`'s ADR-0002 itself.
