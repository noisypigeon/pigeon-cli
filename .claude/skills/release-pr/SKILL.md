---
name: release-pr
description: Use this skill when making a change to a pigeon-tf Terraform module and getting it merged to main. Covers branching off main, committing, opening the pull request, applying the required release:* label, merging, and syncing local main afterward. Trigger on requests like "open a PR for this", "release this module change", "ship this to main", or "merge this pigeon-tf change".
---

# Releasing a pigeon-tf module change

pigeon-tf has no human reviewer and no branch-protection rules — this skill *is*
the PR discipline. Follow it in full for every change, including trivial ones,
because the PR body becomes this module's changelog entry and GitHub Release
notes verbatim, and the label drives the version bump. There is no separate
"write good release notes" step later — get the PR body right the first time.

## Steps

1. **Branch off `main`.**
   ```
   git checkout main
   git pull
   git checkout -b <descriptive-branch-name>
   ```

2. **Make the change and commit it.** Commit message content is not what
   drives the changelog — the PR title and body are. Write commits for
   git history, not for release notes.

3. **Open the PR with a body written for its second life as a changelog entry
   and release note**, not just for a reviewer:
   - Title: short, imperative, human-readable — it becomes the changelog
     section heading verbatim (e.g. "Add versioning input to object-bucket").
   - Body: written in full sentences a consumer of the module would want to
     read later, describing what changed and why it matters to someone
     consuming this module — not "fixed bug" or a raw commit list. It is
     copied verbatim into `digitalocean/<module>/CHANGELOG.md` and into the
     GitHub Release notes by `module-release.yml` on merge, with no editing
     pass in between.
   ```
   gh pr create --title "<title>" --body "<body>" --base main
   ```

4. **Apply exactly one `release:*` label before merging** — this determines
   the version bump for every module changed in this PR:
   - `release:major` — breaking change to a module's inputs/outputs/behavior.
   - `release:minor` — backwards-compatible addition.
   - `release:patch` — fix or internal-only change. This is also what
     `module-release.yml` defaults to if no `release:*` label is present, so
     applying it explicitly isn't strictly required to get a release, but do
     it anyway — an unlabeled PR is a signal something was skipped, not a
     deliberate patch decision.
   - A PR that doesn't touch any `digitalocean/<module>/` directory (e.g.
     workflow or root-doc changes) doesn't need a label at all — nothing will
     be tagged or released regardless.
   ```
   gh pr edit <pr-number> --add-label "release:patch"
   ```

5. **Merge the PR** once ready (no separate review gate exists — this is the
   discipline checkpoint):
   ```
   gh pr merge <pr-number> --squash --delete-branch
   ```
   Merging triggers `module-release.yml` automatically (tags the changed
   module(s), updates their `CHANGELOG.md`, cuts a GitHub Release) and, on any
   resulting `.tf` changes, `module-docs.yml` (regenerates the module's
   `README.md` input/output table). Neither needs to be triggered manually.

6. **Sync local `main`.** "Run on main" here means syncing your local clone
   after the automation has landed its own commits — not re-running any
   workflow manually:
   ```
   git checkout main
   git pull
   ```
   Pulling after merge picks up both the squash-merge commit and the
   automated changelog/docs commits that landed on top of it.
