# pigeon

Authenticate, sink, and transform personal data from external services — starting with email.

`pigeon` connects to an IMAP account (Gmail, Fastmail, iCloud, or Proton Mail Bridge), downloads every message read-only, and transforms it into a flat tree of Markdown files with YAML frontmatter, one file per message, attachments included. It can optionally upload the result to any S3-compatible bucket alongside the local copy.

## Features

- IMAP backup for Gmail, Fastmail, iCloud, and Proton Mail Bridge, authenticated with an app/bridge password (no OAuth setup required).
- Read-only against the mail server (`EXAMINE` + `BODY.PEEK[]`) — never marks messages as read, never deletes anything server-side.
- Converts each message to Markdown (HTML mail included) with frontmatter tags for mailbox, sender domain, year, and identity.
- Deduplicates byte-identical attachments and whole messages across mailboxes.
- Concurrent, resumable sync across mailboxes, with live progress bars.
- Optional upload to an S3-compatible bucket (DigitalOcean Spaces, MinIO, and similar), change-detected so re-runs only upload what's actually different.

The full design rationale for every decision above lives in [`docs/adr/`](docs/adr/), as a sequence of architecture decision records.

## Install

```sh
cargo install pigeon-cli
```

This installs a binary named `pigeon`.

## Usage

Authenticate an identity (prompts for an app/bridge password):

```sh
pigeon email authenticate first.last@example.com
```

List authenticated identities:

```sh
pigeon email list
```

Sync everything for one identity to a local directory (defaults under your OS temp directory if `--local-output` is omitted):

```sh
pigeon email sync first-last --local-output ~/Backups/first-last
```

Also upload to a configured S3-compatible bucket:

```sh
pigeon dataops bucket-config new backup
pigeon email sync first-last --local-output ~/Backups/first-last --remote-output backup
```

Run `pigeon --help`, `pigeon email --help`, or `pigeon dataops --help` for the full command reference.

## License

Licensed under the GNU General Public License v3.0 or later — see [`LICENSE`](LICENSE).
