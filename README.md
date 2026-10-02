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

## Run via Docker

An alternative to installing Rust/ffmpeg locally: build and run `pigeon`
in a container that bundles everything it needs (see ADR-0087). Image
targets `linux/arm64` only.

```sh
mise run docker-build                       # docker build --platform linux/arm64 -t pigeon-cli .
mise run docker-run -- keyring list         # docker run ... pigeon-cli keyring list
```

`/data` inside the container is the single mount point for config,
logs, and job output — `docker-run`'s named volume (`pigeon-data`)
persists it across runs. `PIGEON_CONFIG_DIR` and `PIGEON_LOG_DIR` are
pre-set to `/data/config`/`/data/logs`.

Because a container restart doesn't preserve the OS keyring (see
ADR-0085's Linux caveat), non-interactive/restarted use works in two
phases:

1. **One-time setup**: run `pigeon keyring add <kind> <alias>` once to
   populate `keyring.toml`'s non-secret metadata — it persists in the
   `/data` volume.
2. **Per-run**: supply the actual secret via a `PIGEON_SECRET_<ALIAS>`
   environment variable (alias uppercased, non-alphanumeric characters
   replaced with `_`) — `pigeon` reads it directly instead of the OS
   keyring, e.g.:

   ```sh
   docker run --platform linux/arm64 --rm -v pigeon-data:/data \
     -e PIGEON_SECRET_MY_ALIAS=<secret> \
     pigeon-cli job run email-sync
   ```

How that env var gets populated (a secrets manager, CI variable, etc.)
is left to your own infrastructure.

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
