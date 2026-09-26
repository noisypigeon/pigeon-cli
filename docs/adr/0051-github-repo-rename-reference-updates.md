# ADR-0051: GitHub repo rename (`pigeon-cli` → `pigeon`) reference updates

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

The GitHub repository has been renamed from `noisypigeon/pigeon-cli` to
`noisypigeon/pigeon`. This is a repo-identity change only — it does **not**
touch the crates.io package name (`pigeon-cli`, ADR-0018), the
`service/pigeon-cli/` directory, or the `pigeon` binary name. Those were
independent naming decisions made for unrelated reasons and are not being
revisited here.

A full repo sweep confirmed exactly what does and doesn't need to change:

- `service/pigeon-cli/Cargo.toml`'s `repository` field
  (`https://github.com/noisypigeon/pigeon-cli`) is the one reference that
  actually ships to crates.io users — it's what renders on the crate's
  crates.io page and in `cargo info`. This is the primary reason a release is
  needed.
- `service/pigeon-cli/README.md` (the crates.io-facing README, ADR-0050)
  has two live links built from the old repo URL.
- Root `README.md`'s title, `# pigeon-cli`, is the repo-orientation doc's
  (ADR-0050) own self-identification and should track the repo's actual name.
- `terraform/README.md`'s example `git clone` command, two downstream
  local-path mentions, and its "Releases are tagged on `pigeon-cli`'s `main`"
  prose line all use the old repo name.
- `docs/adr/*.md` and both `CHANGELOG.md` files (root and
  `service/pigeon-cli/`) contain roughly 70 historical
  `github.com/noisypigeon/pigeon-cli/{issues,pull}/N` links, accumulated
  across every prior ADR's "Out of scope" bullets and changelog entries.
  GitHub redirects renamed-repo URLs automatically, so none of these are
  actually broken, but the project owner chose to rewrite them rather than
  rely on the redirect indefinitely.
- The local `origin` git remote itself still points at the old URL — not a
  tracked file, but needs updating alongside this work.

Three spots are narrative historical fact rather than links, and are **not**
rewritten by the bulk pass — two are in ADR-0018, which already carries a
precedent for handling this (its 2026-09-26 Amendment for the ADR-0050
manifest relocation), and one is in ADR-0050:

- ADR-0018's Context bullet: `` `git remote -v` confirms the canonical repo is `github.com/noisypigeon/pigeon-cli`. `` — true when written, and stays true as a record of what `git remote -v` showed at the time.
- ADR-0018's `repository = "https://github.com/noisypigeon/pigeon-cli"` code block documenting its own original Cargo.toml decision.
- ADR-0050's citation of the exact URL it decided `service/pigeon-cli/README.md`'s license link should contain — a record of that decision, not a dangling reference link (the actual README link, per this ADR, is now updated).

Rewriting historical issue/PR links is treated as a distinct, narrower
exception to this project's general "don't retroactively edit historical ADR
content" convention (ADR-0017, ADR-0036): those links are pure addresses to
still-live, unchanged GitHub objects (the issue/PR numbers and their content
don't change), not edits to any decision's substance — closer to fixing a
moved-file link than to rewriting history.

Explicitly confirmed **unchanged**: the crate/package name `pigeon-cli`
(ADR-0018), the `service/pigeon-cli/` directory, the `pigeon` binary name,
`CHANGELOG.md`'s `[pigeon-cli]` changelog scope-tag convention (ADR-0050),
`LICENSE.md`'s GPL boilerplate program-name notice, and every reference to
the *separate* `pigeon-tf` repo (ADR-0037/0038's origin citations, and
`terraform/modules/digitalocean/droplet/access_key.tf`'s module source URL) —
a different repo entirely, untouched by this rename.

## Decision

### Local git remote

`origin` is repointed at the renamed repo:

```
git remote set-url origin git@github.com:noisypigeon/pigeon.git
```

Not a file change, but required for `git fetch`/`push` to address the repo
by its current name rather than relying on GitHub's redirect indefinitely.

### `service/pigeon-cli/Cargo.toml`

`repository` becomes `https://github.com/noisypigeon/pigeon`. `version`
bumps `0.2.0` → `0.2.1` — a patch release, since the only change is corrected
metadata, in preparation for the next `cargo publish`.

### `service/pigeon-cli/README.md`

Its two `noisypigeon/pigeon-cli` links (the `docs/adr/` tree link and the
`LICENSE.md` blob link) become `noisypigeon/pigeon`.

### Root `README.md`

Retitled `# pigeon-cli` → `# pigeon`, matching the repo's new identity as the
doc that orients readers to the whole repo (ADR-0050).

### `terraform/README.md`

The example `git clone git@github.com:noisypigeon/pigeon-cli.git ../pigeon-cli`
command and its two downstream `../pigeon-cli` local-path mentions become
`../pigeon`, for consistency with the actual renamed repo.

### Historical issue/PR links

Every `github.com/noisypigeon/pigeon-cli/issues/N` and `.../pull/N` link
across `docs/adr/*.md`, `/CHANGELOG.md`, and `service/pigeon-cli/CHANGELOG.md`
is rewritten to `github.com/noisypigeon/pigeon/...` — a literal substring
replace of `noisypigeon/pigeon-cli` → `noisypigeon/pigeon`, excluding the two
ADR-0018 narrative-fact spots above.

### ADR-0018 amendment

A new Amendment section is appended to `docs/adr/0018-crates-io-publishing.md`
recording the rename and pointing to this ADR, following the same
amendment-section pattern ADR-0018 already uses for the ADR-0050 manifest
relocation.

## Consequences

- crates.io's rendered `repository` link for `pigeon-cli` becomes correct
  once `0.2.1` is published.
- Every in-repo GitHub link points at the current repo name instead of
  relying on GitHub's redirect.
- `cargo publish`/`publish-dry-run` itself is **not** run by this ADR's
  implementation — per ADR-0018's existing "publishing stays manual" decision,
  actually pushing `0.2.1` to crates.io is a follow-up step the project owner
  runs themselves once ready.
- The crate/package name, binary name, and `service/pigeon-cli/` directory
  stay `pigeon-cli`/`pigeon`/`service/pigeon-cli/` — this ADR does not touch
  any of them, so the GitHub repo name and the crate name now intentionally
  differ (`pigeon` vs. `pigeon-cli`), on top of the binary name (`pigeon`)
  already differing from the crate name per ADR-0018.

## Out of scope

- Renaming the crate/package (`pigeon-cli`), the `service/pigeon-cli/`
  directory, or the `pigeon` binary — no such request, and ADR-0018's
  reasoning for the crate name is unaffected by the GitHub repo's name.
- Actually running `cargo publish` — a manual follow-up step, not part of
  this ADR's implementation.
- Any reference to the separate `pigeon-tf` repo — unrelated, not renamed.
- Implementation itself — like every ADR before it, this is a decision
  record only.
