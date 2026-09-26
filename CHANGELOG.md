# Changelog

One line per PR across this whole repo, sectioned by date, newest first.
Not versioned — for versioned, package-scoped changelogs see
[`service/pigeon-cli/CHANGELOG.md`](service/pigeon-cli/CHANGELOG.md) (the
`pigeon-cli` crate) and `terraform/modules/*/*/CHANGELOG.md` (each
Terraform module). Entry format: `- [<scope>] <summary> ([#N](PR URL))`,
where `<scope>` is `pigeon-cli`, `terraform/<provider>/<module>`, or `repo`
for cross-cutting/structural changes. Starts fresh at ADR-0050 — no
backfill of prior history.

## 2026-09-26

- [repo] ADR-0050: relocate `Cargo.toml`/`Cargo.lock` into `service/pigeon-cli/`, add this repo-wide dated changelog, rename `LICENSE` to `LICENSE.md`, and rewrite both READMEs ([#59](https://github.com/noisypigeon/pigeon/pull/59)).
