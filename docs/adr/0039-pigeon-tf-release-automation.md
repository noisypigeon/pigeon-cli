# ADR-0039: pigeon-tf release automation

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.
- **Origin**: pigeon-tf ADR-0002, merged into this repo by ADR-0037.

## Context

ADR-0038 explicitly deferred CI/release automation ("Out of scope: CI or release automation") — reasonable when the repo had four modules and a manual `git tag` + `gh release create` per whole-repo version was low toil. That toil is growing in two ways: modules now need their own input/output documentation (none currently has a `README.md`), and whole-repo `vX.Y.Z` tagging (`v0.1.0`–`v0.1.3`, all four confirmed repo-wide) means every module gets a version bump even when only one actually changed.

This ADR reverses that specific "out of scope" line and designs the automation: a GitHub Actions workflow that generates per-module README input/output docs, tags/releases modules independently, and maintains a per-module `CHANGELOG.md` from PR descriptions — plus a Claude Code skill giving the sole contributor a repeatable branch → PR → merge → sync loop that feeds the automation correctly (labels, PR body content). PR discipline is wanted here even without another human reviewer, so the workflow leans on PR metadata (labels, body) as its structured input rather than parsing commit messages or requiring a separate release-config file to hand-edit.

This is greenfield: no `.github/workflows/` exists in `pigeon-tf`, `pigeon-do`, or `pigeon-cli`; no terraform-docs, changelog, or release tooling is referenced anywhere in the org; no `.claude/skills/` exists anywhere (global or in any sibling repo) to draw conventions from. The decisions below were made fresh.

## Decision

### Module documentation (terraform-docs)

- `terraform-docs` (via the `terraform-docs/gh-actions` GitHub Action, `output-method: inject`) generates an inputs/outputs table only — not full resource/provider dumps — into each module's own `README.md`, between `<!-- BEGIN_TF_DOCS -->`/`<!-- END_TF_DOCS -->` markers. This creates a `README.md` in every module directory (any directory containing a `versions.tf`, per ADR-0038's self-containment convention) where none currently exists.
- The `terraform/README.md`'s existing two-column `Modules` table stays as-is, as the lightweight repo-wide index.
- Runs as `.github/workflows/module-docs.yml`, triggered on `push` to `main` path-filtered to `terraform/modules/digitalocean/**/*.tf`, auto-committing doc changes directly to `main`.

### Per-module versioning and tagging

- Tags become path-scoped going forward: `digitalocean/<module>/vX.Y.Z`, one independent version sequence per module.
- The bump level (major/minor/patch) is signaled by a required PR label — `release:major`, `release:minor`, or `release:patch` — applied before merge; defaults to `patch` if none is present. Chosen over conventional-commit-title parsing or an always-patch default: it's explicit, requires no parsing convention to remember, and is easy for a solo maintainer to set correctly per PR.
- The four existing whole-repo tags (`v0.1.0`–`v0.1.3`) are frozen as historical, untouched. Each module's own scoped versioning restarts at `v0.1.0`, but only lazily — the first time that specific module actually changes after this ADR lands, not backfilled or retagged for all four modules immediately.
- Runs as `.github/workflows/module-release.yml`, triggered on `pull_request` `closed` targeting `main` with `if: github.event.pull_request.merged == true`. This trigger gives direct access to the PR's labels and body without extra API calls, and works regardless of merge strategy (merge/squash/rebase), since `merge_commit_sha` is populated on the event either way. For each changed module — a directory containing a `versions.tf` with files differing between `base.sha` and `merge_commit_sha` — the workflow computes the next version from the label, creates the scoped tag, and creates a GitHub Release against that tag with the PR body as release notes.

### Per-module CHANGELOG.md

- One `CHANGELOG.md` per module directory (e.g. `terraform/modules/digitalocean/object-bucket/CHANGELOG.md`), not a single repo-root file — matches per-module versioning one-to-one, and keeps each module's history self-contained next to its `.tf` files.
- Keep a Changelog-style format. Each release workflow run prepends an entry: version, date, the PR title, the PR body verbatim, and a link back to the PR. The file is created on that module's first automated release (lazily, per the cutover above) — it doesn't exist for any module today.

### Claude Code skill for the PR workflow

- A new `.claude/skills/<name>/SKILL.md` (exact name decided at implementation time) covers the full loop: branch off `main` → commit → `gh pr create` → apply exactly one `release:*` label → merge → sync local `main` (`git checkout main && git pull`) once merged.
- The PR body created by this skill should be written for the changelog/release notes it becomes, not just for a reviewer — the release workflow copies it verbatim into the module's `CHANGELOG.md` entry and the GitHub Release notes.
- "Run on main" resolves to a local sync step (checkout + pull) after merge, not a manual workflow re-trigger — the release/docs workflows are event-triggered and don't need manual dispatch in the normal path.
- Frontmatter follows Anthropic's bundled `skill-development` reference skill (the only convention reference available, since no project- or user-authored skill exists anywhere in this org to mirror): minimal required fields `name` + a third-person, trigger-phrase-driven `description`.
- Implementation will need new `Bash` allow-entries in `.claude/settings.local.json` — it currently pre-approves only `git add/commit/tag/push` and `gh release`, not `gh pr *`, `git checkout *`, `git branch *`, or `gh label *` — so the skill can run its full loop without repeated permission prompts.

## Consequences

- Documentation, versioning, and changelog toil is automated per module instead of manually maintained per whole-repo release.
- Two new GitHub Actions workflows exist where none did before, directly reversing part of ADR-0038's original CI/release-automation deferral.
- The solo contributor gets a PR ritual enforced by convention (the skill) rather than by GitHub — this repo has no branch protection today, and this ADR doesn't add any.
- Automated bot commits (doc regeneration, changelog updates) become a routine part of `main`'s history.
- `pigeon-do` currently pins `pigeon-tf` by checking out a single whole-repo tag (per `pigeon-do`'s own ADR-0002). Once new changes stop landing under whole-repo tags, there's no longer one tag that captures "all modules as of now" — `pigeon-do`'s consumption model will need its own follow-up decision (e.g. tracking `main` directly, since its existing model already accepts version drift, or pinning per module). This ADR flags that consequence but explicitly does not resolve it here.

## Out of scope

- Branch protection rules or required status checks — the repo has none today, and this ADR doesn't add them; PR discipline here is convention-enforced via the skill, not GitHub-enforced.
- `terraform fmt`/`validate` CI gates on PRs — a natural future companion, not part of what was asked here.
- Retroactively tagging or changelog-ing the three modules that haven't changed since the legacy whole-repo tags.
- Any change to `pigeon-do` itself, including how it will eventually consume per-module tags.
