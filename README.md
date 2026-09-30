# pigeon

`pigeon`, a Rust CLI that authenticates, syncs, transforms, and optionally
encrypts personal data to local storage or S3-compatible remotes.

## Structure

- [`service/pigeon-cli/`](service/pigeon-cli/) — the Rust CLI. See its own
  [README](service/pigeon-cli/README.md) for the command reference.
- [`docs/adr/`](docs/adr/) — architecture decision records governing every
  change in this repo.
- [`docs/report/`](docs/report/) — job-run log analysis reports produced
  by the `analyze-job-run` Claude Code skill.

## Getting started

This repo uses [mise](https://mise.jdx.dev/) as the single entry point for
all Rust tooling — it wires up the toolchain and runs `cargo` against
`service/pigeon-cli/`'s manifest, so these work unchanged from the repo
root:

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

This installs a binary named `pigeon`. See
[`service/pigeon-cli/README.md`](service/pigeon-cli/README.md) for the full
command reference.

## License

Licensed under the GNU General Public License v3.0 or later — see [`LICENSE.md`](LICENSE.md).
