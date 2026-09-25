# pigeon

Transform personal data from external services.

`pigeon` provides a job scheduler and keyring for data set batch operations, with support for checkpoints, concurrent operations, and file encryption.

## Features

List coming soon!

The full design rationale for every decision above lives in [`docs/adr/`](docs/adr/), as a sequence of architecture decision records.

## Install

```sh
cargo install pigeon-cli
```

This installs a binary named `pigeon`.

## Usage

Add an email identity, bucket config, or encryption key:

```sh
pigeon keyring add
```

List keys:

```sh
pigeon keyring list
```

Sync emails:

```sh
pigeon job run email-sync
```

Decrypt files:

```sh
pigeon job run decrypt-files
```

Run `pigeon --help`, `pigeon job --help`, or `pigeon keyring --help` for the full command reference.

## License

Licensed under the GNU General Public License v3.0 or later — see [`LICENSE`](LICENSE).
