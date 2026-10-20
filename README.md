# pigeon

`pigeon`, a Rust CLI that imports, transcodes, and deduplicates personal
data into S3-compatible remotes. It provides a job runner and a keyring for
batch operations over data sets, with support for checkpointed resume,
concurrent operations, and rate-limited transfers.

## Structure

- [`src/`](src/) — the Rust CLI source.
- [`adr/`](adr/) — architecture decision records governing every
  change in this repo.
- [`fetch-lib.sh`](fetch-lib.sh) / [`.lib-version`](.lib-version) — fetch
  [`noisypigeon/library`](https://github.com/noisypigeon/library), which holds
  this repo's release pipeline, local observability stack and Claude Code
  skills, at a pinned version (CLI-0161).

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

An alternative to installing the converters locally: run `pigeon` in a
container that bundles everything its jobs shell out to (CLI-0087,
CLI-0155, CLI-0163) — `rclone`, a static `ffmpeg` 9, LibreOffice, Calibre,
pandoc and Ghostscript. Every release publishes it for `linux/arm64` and
`linux/amd64` to `ghcr.io/noisypigeon/pigeon-cli`, tagged `X.Y.Z`, `X.Y`
and `latest`.

```sh
mise run docker-pull                        # pull :latest (or `-- 0.4.56`) and tag it pigeon-cli
mise run docker-build                       # or build it locally, for this host's architecture
mise run docker-run -- keyring list         # docker run ... pigeon-cli keyring list
```

`/data` inside the container is the single mount point for config,
logs, and job output — `docker-run`'s named volume (`pigeon-data`)
persists it across runs. `PIGEON_CONFIG_DIR` and `PIGEON_LOG_DIR` are
pre-set to `/data/config`/`/data/logs`, so `keyring.toml` — secrets
included, since CLI-0135 — lives at `/data/config/keyring.toml`, and the
`rclone.conf` pigeon generates beside it lands there too. Treat the volume
as secret material.

A `PIGEON_SECRET_<ALIAS>` environment variable (alias uppercased,
non-alphanumeric characters replaced with `_`) still overrides the stored
secret for one run, when you'd rather inject it from a secrets manager:

```sh
docker run --rm -v pigeon-data:/data \
  -e PIGEON_SECRET_MY_ALIAS=<secret> \
  ghcr.io/noisypigeon/pigeon-cli job run email import
```

The entrypoint is `tini -- pigeon`, so the converters a job spawns are
reaped and `docker stop` reaches `pigeon` as a signal.

## Local observability stack (Grafana/Prometheus/Loki)

Previously, `pigeon` job runs on Scaleway compute were observed via
Scaleway's managed Cockpit (see CLI-0092/0093), fed by an on-host Grafana
Alloy agent. Running `pigeon` bare-metal on your own machine instead (see
CLI-0128) drops that path entirely — nothing ships `/metrics` or
`pigeon.jsonl` anywhere by default. The library's
[`observability/pigeon-cli`](https://github.com/noisypigeon/library/tree/main/observability/pigeon-cli)
provides an equivalent local stack via Docker Compose, at the version
`.mise.toml`'s `PIGEON_OBSERVABILITY_VERSION` pins (CLI-0161).

```sh
mise run observability-up      # fetches the pinned stack, starts Prometheus, Loki, Promtail, Grafana
```

Then run `pigeon` commands normally, bare-metal, alongside the stack —
e.g. `pigeon job run transform ...`. Open
[http://localhost:3000](http://localhost:3000) for Grafana (default
credentials `admin`/`admin` on first login — change them) and open the
pre-provisioned "pigeon-cli overview" dashboard.

`mise run observability-down` stops the stack; Prometheus/Grafana data
persists in named Docker volumes across restarts. `mise run
observability-logs` tails the stack's own container logs (not pigeon's),
for debugging the stack itself.

Known v1 limitation: no host-wide metrics (CPU/mem/disk for the whole
machine, not just pigeon's own process tree) — see CLI-0128.

## Commands

- `pigeon keyring add [email|bucket]` — authenticate an email identity, or configure an S3-compatible bucket.
- `pigeon keyring modify [alias]` — edit an existing entry's fields.
- `pigeon keyring delete <alias>` — remove a configured entry and its secret.
- `pigeon keyring list` — list every configured entry.
- `pigeon job run email import [flags]` — deliver each message's raw `.eml` and unpacked attachments from an authenticated identity to a destination, at a chosen `--messages-per-minute` pace. What has been delivered is recorded in a durable ingest ledger in `--ledger-bucket`, so a run on a fresh host resumes correctly and downstream jobs can rewrite the destination without anything being ingested twice; `--legacy` keeps the older local-checkpoint-only behavior.
- `pigeon job run email scan [flags]` — compare each identity's mailboxes against its ingest ledger without fetching any message, and write per-mailbox counts of new messages and attachments to `scan-status.tsv` in `--ledger-bucket`. Cheap enough to run on a schedule.
- `pigeon job run deduplicate [flags]` — inflate every zip, content-hash a bucket, keep one copy of each byte-identical file, and upload the result.
- `pigeon job run transform [flags]` — normalize one input family per run between a source and destination bucket: images (`png`/`jpeg`/`heic`) and video (`mov`/`m4v`/`mp4`) via `ffmpeg` at a chosen `--video-quality`; audio (`mp4-audio`/`m4a`/`mp3`/`aac`/`wav`/`opus`) to Opus at a chosen `--audio-quality`; spreadsheets (`numbers`/`xlsx`/`xls`/`csv`/`tsv`) to one `.csv` per non-empty sheet; documents (`doc`/`docx`/`pages`/`rtf`/`pdf`/`epub`/`pptx`/`key`) to `.pdf`; text (`txt`/`md`/`markdown`/`mkd`/`rst`/`textclipping`) to `.txt`; more raster images (`avif`/`tif`/`tiff`/`webp`/`bmp`) to `.jpg` with transparency flattened onto white; icons and artwork (`ico`/`icns`/`procreate`/`procreate-2`) to `.png` with transparency kept, a Procreate document at full canvas resolution; vector drawings (`ai`/`emz`/`eps`/`svg`) to `.svg`; and calendars and contacts (`ics`/`vcf`) to readable `.txt`. Needs `soffice` for the LibreOffice-readable kinds (including `ai`/`emz`), `ebook-convert` for `epub` and `pandoc` for `md`/`rst` and Ghostscript (`gs`) for `eps`; a file already in its target format (`jpeg`, `csv`, `pdf`, `txt`, `svg`) is copied through byte-for-byte. A `.key` that is not an iWork container — that extension is also conventional for a private key — is refused rather than converted.
- `pigeon job run rclone copy [flags]` — copy one configured bucket to another via `rclone`.
- `pigeon job run rclone delete [flags]` — recursively and permanently purge a configured bucket's contents.

Every job takes `--debug-log-bucket` (where its report, log and transcript are uploaded; formerly `--report-bucket`) and accepts `--ledger-bucket`, which only the email jobs use so far.

Run `pigeon --help`, `pigeon keyring --help`, or `pigeon job --help` for the full command reference.

The full design rationale for every decision behind this crate lives in [`adr/`](adr/), as a sequence of architecture decision records.

## License

Licensed under the GNU General Public License v3.0 or later — see [`LICENSE.md`](LICENSE.md).
