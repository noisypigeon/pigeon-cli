# syntax=docker/dockerfile:1
# ADR-0087: Dockerizes pigeon-cli. linux/arm64 only -- see the ADR for why.
# Build/run with `docker build/run --platform linux/arm64` (see the `mise
# run docker-build`/`docker-run` tasks); platform is intentionally not
# pinned per-stage here so it's set once, from the command line.

FROM rust:1.92-slim-bookworm AS builder

WORKDIR /build
# async-native-tls (IMAP) pulls in native-tls -> openssl-sys on Linux,
# which needs pkg-config + OpenSSL dev headers to build -- neither is in
# the base rust image. ADR-0148 removed `minio`, which used to be named
# here too; this is still required for the IMAP half alone, so neither the
# apt-get below nor the runtime libssl3 may be dropped with it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release

FROM debian:bookworm-slim

# The three LibreOffice packages back `job run transform`'s `soffice`
# kinds: `-calc` reads `.numbers` (ADR-0149), and `-writer`/`-impress` read
# the document kinds ADR-0154 added. The split matters -- `-calc` alone does
# **not** open Writer or Impress formats, so without the other two, six
# document kinds would fail on this image.
#
# `calibre` provides `ebook-convert` for `--input-file-type epub` alone:
# LibreOffice's EPUB filter is export-only (measured against its own filter
# registry), so it is the one document kind `soffice` cannot serve.
# `pandoc` renders `md`/`rst` to prose.
#
# Together these are by far the heaviest thing in this image. Accepted
# rather than worked around: the alternative is a job that cannot run where
# it is deployed. The in-process kinds -- `xlsx`/`xls`/`csv`/`tsv`/`pdf`/
# `txt`/`textclipping` -- need none of them.
#
# Still missing and still open: `rclone`, which *every* job requires on
# PATH -- https://github.com/noisypigeon/pigeon-cli/issues/96.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ffmpeg \
       libreoffice-calc libreoffice-writer libreoffice-impress \
       calibre pandoc \
       ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /usr/sbin/nologin pigeon
COPY --from=builder /build/target/release/pigeon /usr/local/bin/pigeon

ENV PIGEON_CONFIG_DIR=/data/config
ENV PIGEON_LOG_DIR=/data/logs
VOLUME /data
RUN mkdir -p /data && chown pigeon:pigeon /data

USER pigeon
WORKDIR /data
ENTRYPOINT ["pigeon"]
