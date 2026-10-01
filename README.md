# pigeon

`pigeon`, a Rust CLI that authenticates, syncs, transforms, and optionally
encrypts personal data to local storage or S3-compatible remotes. It
provides a job scheduler and keyring for data set batch operations, with
support for checkpoints, concurrent operations, and file encryption.

## Structure

- [`src/`](src/) — the Rust CLI source.
- [`docs/adr/`](docs/adr/) — architecture decision records governing every
  change in this repo.
- [`docs/report/`](docs/report/) — job-run log analysis reports produced
  by the `analyze-job-run` Claude Code skill.

## Getting started

This repo uses [mise](https://mise.jdx.dev/) as the single entry point for
all Rust tooling:

```sh
mise run build              # build the pigeon binary
mise run pigeon -- <args>   # run it, e.g. `mise run pigeon -- keyring list`
mise run test                # run the test suite
mise run fmt                 # format
mise run fmt-check           # check formatting
mise run lint                 # clippy, warnings denied
mise run ci                   # the full local gate (fmt-check + lint + test)
```

## Install (published crate)

```sh
cargo install pigeon-cli
```

This installs a binary named `pigeon`.

## Commands

- `pigeon keyring add [email|bucket|encryption-key]` — authenticate an email identity, configure an S3-compatible bucket, or register a symmetric encryption key.
- `pigeon keyring modify [alias]` — edit an existing entry's fields.
- `pigeon keyring delete <alias>` — remove a configured entry and its secret.
- `pigeon keyring list` — list every configured entry.
- `pigeon job run email-sync [flags]` — fetch, transform, deduplicate, and optionally upload mail for one or more authenticated identities.
- `pigeon job run decrypt-files [flags]` — decrypt every `*.enc` file under an input directory into an output directory.

Run `pigeon --help`, `pigeon keyring --help`, or `pigeon job --help` for the full command reference.

The full design rationale for every decision behind this crate lives in [`docs/adr/`](docs/adr/), as a sequence of architecture decision records.

## License

Licensed under the GNU General Public License v3.0 or later — see [`LICENSE.md`](LICENSE.md).
