# pigeon-cli

Transform personal data from external services.

`pigeon` provides a job scheduler and keyring for data set batch operations, with support for checkpoints, concurrent operations, and file encryption.

## Install

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

The full design rationale for every decision behind this crate lives in [`docs/adr/`](https://github.com/noisypigeon/pigeon/tree/main/docs/adr), as a sequence of architecture decision records.

## License

Licensed under the GNU General Public License v3.0 or later — see [`LICENSE.md`](https://github.com/noisypigeon/pigeon/blob/main/LICENSE.md).
