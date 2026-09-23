# ADR-0018: publish `pigeon` to crates.io

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.

## Context

`pigeon` is to be published to crates.io via `cargo publish`. Several concrete, consequential facts shape this decision, confirmed directly rather than assumed:

- **The package name `pigeon` is already taken** on crates.io — an unrelated big-endian binary-packing crate (`gitlab.com/tinytown/pigeon`), confirmed via the crates.io API. The registry package needs a different name.
- **No `LICENSE` file and no `README.md` exist** in the repo today — both are required (license) or effectively required (readme — `cargo publish` errors if a configured `readme` path doesn't exist) for a real crates.io release.
- `git remote -v` confirms the canonical repo is `github.com/noisypigeon/pigeon-cli`.
- **No CI/CD automation exists** — no `.github/workflows/` directory anywhere in this project's history. Every existing task (`build`, `pigeon`, `test`, `fmt`, `lint`, `ci`) is a local `mise run <task>` per ADR-0004. Publishing should follow that same established pattern rather than introducing CI/CD as an unrequested side effect.
- All current dependencies (`async-imap`, `tokio`, `clap`, `serde`, `minio`, `keyring`, etc.) are ordinary crates.io dependencies with version requirements — none are git/path deps, which would block publishing.
- The crate is already a lib+bin layout (`src/lib.rs` + `src/main.rs`), which crates.io/`cargo install` fully supports without restructuring.

Three decisions were confirmed directly with the project owner before writing this ADR:
- **Package name**: `pigeon-cli`, matching the existing GitHub repo name exactly. The installed **binary** stays named `pigeon`, decoupled from the package name via an explicit `[[bin]]` section — preserving ADR-0002/ADR-0004's established `pigeon` command surface.
- **License**: GPL-3.0, as `GPL-3.0-or-later` (the FSF-recommended SPDX identifier when "GPLv3" is specified without further qualification).
- **Public release confirmed**: this makes the full source, and every published version, permanently publicly downloadable (distinct from already being on GitHub — crates.io versions can be yanked but never deleted).

## Decision

### Package name `pigeon-cli`; binary stays `pigeon`

`Cargo.toml`'s `[package] name` becomes `"pigeon-cli"`. A new explicit section:

```toml
[[bin]]
name = "pigeon"
path = "src/main.rs"
```

keeps `cargo install pigeon-cli` installing a binary still invoked as `pigeon`, so `mise run pigeon -- <args>` and every other ADR-0002/ADR-0004-established convention is unaffected.

### License: `GPL-3.0-or-later`

`Cargo.toml` gains `license = "GPL-3.0-or-later"`. A new `LICENSE` file at the repo root holds the full GPL-3.0 text. Dependency license compatibility — nearly all Rust-ecosystem crates are MIT/Apache-2.0, which combines cleanly into a GPL-3.0 work — is a pre-publish verification step, not exhaustively audited by this ADR itself.

### New required/recommended `Cargo.toml` metadata

```toml
repository = "https://github.com/noisypigeon/pigeon-cli"
readme = "README.md"
keywords = ["email", "imap", "s3", "backup", "cli"]
categories = ["command-line-utilities", "email"]
```

(`keywords` is capped at 5 by crates.io; both `categories` entries are valid existing crates.io category slugs.)

### New `README.md`

Required by the `readme` field above, and it's what renders on the crate's crates.io page: install via `cargo install pigeon-cli`, a brief usage pointer (`pigeon email authenticate`/`sync`, `pigeon dataops bucket-config new`, etc.), and a link to `docs/adr/` for design rationale. Content scope only — not spelled out line-by-line in this ADR.

### Publishing stays manual and `mise`-driven

Matching this project's existing no-CI-service pattern, two new `.mise.toml` tasks:

- `publish-dry-run` — `cargo publish --dry-run`, safe and repeatable verification.
- `publish` — `cargo publish`, the real, irreversible-per-version action.

`cargo login` with a crates.io API token is a one-time, manual, out-of-band step the developer performs themselves — never stored or scripted in the repo.

### Version stays `0.1.0` for the first publish

Future publishes require manually bumping `Cargo.toml`'s `version` beforehand (crates.io rejects re-publishing an existing version number). No version-bump tooling (e.g. `cargo-release`) is introduced.

### No `exclude`/`include` list needed

A review found no secrets or personal data in tracked files (test fixtures use `example.com`; ADR narrative text is illustrative, not literal embedded data). crates.io's default `.gitignore`-based packaging is sufficient.

## Consequences

- The published package name (`pigeon-cli`) and the binary name (`pigeon`) intentionally differ — worth stating plainly so it doesn't read as an inconsistency later.
- This is a real, irreversible-per-version public release: source code, dependency list, and every published version become permanently publicly downloadable (yankable, not deletable).
- No CI/CD is introduced by this ADR — publishing remains a manual, local `mise run publish` action by whoever holds the crates.io API token, consistent with every other task in this project.
- A `LICENSE` file and `README.md` are added to the repo root for the first time.

## Out of scope

- GitHub Actions or any other CI/CD automation for publishing — explicitly not introduced, matching this project's existing all-`mise`, no-CI pattern.
- Automated version-bumping tooling (`cargo-release` or similar).
- A full dependency license audit — flagged as a pre-publish verification step, not performed by this ADR.
- Implementation itself — like every ADR before it, this is a decision record only.
