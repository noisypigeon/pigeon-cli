# syntax=docker/dockerfile:1
# CLI-0087: Dockerizes pigeon-cli. linux/arm64 only -- see the ADR for why.
# Build/run with `docker build/run --platform linux/arm64` (see the `mise
# run docker-build`/`docker-run` tasks); platform is intentionally not
# pinned per-stage here so it's set once, from the command line.

FROM rust:1.92-slim-bookworm AS builder

WORKDIR /build
# async-native-tls (IMAP) pulls in native-tls -> openssl-sys on Linux,
# which needs pkg-config + OpenSSL dev headers to build -- neither is in
# the base rust image. CLI-0148 removed `minio`, which used to be named
# here too; this is still required for the IMAP half alone, so neither the
# apt-get below nor the runtime libssl3 may be dropped with it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release

# CLI-0155: `rclone` is the one dependency *every* job needs -- transform
# pulls and pushes with it, deduplicate lists and copies with it, email
# import delivers with it, `rclone copy`/`delete` are it, and CLI-0100's
# report upload uses CopyTo. The image shipped without it until now (#96).
#
# A pinned upstream binary rather than Debian's: bookworm has 1.60.1 (2022)
# and this project is developed against 1.72+. Every flag the code passes
# predates 1.60, so apt would *work* -- but a 15-version skew between where
# behaviour is verified and where it runs is the exact shape of defect this
# project is worst at seeing (CLI-0139, CLI-0140: something resolved to
# nothing and exited 0).
#
# Fetched in a builder stage so curl/unzip never reach the runtime image,
# and verified against upstream's SHA256SUMS -- a substituted or corrupted
# archive must fail the build, not silently ship.
FROM debian:bookworm-slim AS rclone
# Deliberately NOT named RCLONE_VERSION. rclone configures itself from
# RCLONE_<FLAG> environment variables, so a build arg by that name is read
# as `--version` and every rclone invocation in this stage dies with
# `strconv.ParseBool: parsing "1.75.2"`. Found by the smoke test below,
# which is the reason it is there.
ARG PIGEON_RCLONE_VERSION=1.75.2
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /rclone
RUN set -eux; \
    archive="rclone-v${PIGEON_RCLONE_VERSION}-linux-arm64.zip"; \
    base="https://downloads.rclone.org/v${PIGEON_RCLONE_VERSION}"; \
    curl -fsSL -o "$archive" "$base/$archive"; \
    curl -fsSL -o SHA256SUMS "$base/SHA256SUMS"; \
    grep " \*\?$archive\$" SHA256SUMS > expected.sha256; \
    test -s expected.sha256; \
    sha256sum -c expected.sha256; \
    unzip -j "$archive" "*/rclone" -d /rclone/bin; \
    chmod +x /rclone/bin/rclone; \
    /rclone/bin/rclone version

FROM debian:bookworm-slim

# The three LibreOffice packages back `job run transform`'s `soffice`
# kinds: `-calc` reads `.numbers` (CLI-0149), and `-writer`/`-impress` read
# the document kinds CLI-0154 added. The split matters -- `-calc` alone does
# **not** open Writer or Impress formats, so without the other two, six
# document kinds would fail on this image.
#
# `calibre` provides `ebook-convert` for `--input-file-type epub` alone:
# LibreOffice's EPUB filter is export-only (measured against its own filter
# registry), so it is the one document kind `soffice` cannot serve.
# `pandoc` renders `md`/`rst` to prose.
#
# `ghostscript` interprets `--input-file-type eps` to PDF ahead of `soffice`
# (CLI-0160): LibreOffice alone turns a real Illustrator EPS into an empty
# page and exits 0.
#
# Together these are by far the heaviest thing in this image. Accepted
# rather than worked around: the alternative is a job that cannot run where
# it is deployed. The in-process kinds -- `xlsx`/`xls`/`csv`/`tsv`/`pdf`/
# `txt`/`textclipping`/`ics`/`vcf` -- need none of them.
#
# `rclone` itself comes from the pinned builder stage above (CLI-0155), not
# from apt.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ffmpeg \
       libreoffice-calc libreoffice-writer libreoffice-impress \
       calibre pandoc \
       ghostscript \
       ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /usr/sbin/nologin pigeon
COPY --from=builder /build/target/release/pigeon /usr/local/bin/pigeon
COPY --from=rclone /rclone/bin/rclone /usr/local/bin/rclone

ENV PIGEON_CONFIG_DIR=/data/config
ENV PIGEON_LOG_DIR=/data/logs
VOLUME /data
RUN mkdir -p /data && chown pigeon:pigeon /data

USER pigeon
WORKDIR /data
ENTRYPOINT ["pigeon"]
