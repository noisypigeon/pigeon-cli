# syntax=docker/dockerfile:1
# CLI-0087: Dockerizes pigeon-cli. CLI-0163: builds for linux/arm64 and
# linux/amd64 (each stage keys off BuildKit's TARGETARCH) and is published to
# ghcr.io/noisypigeon/pigeon-cli on every release by the library's
# container-image-release.yml. Locally, `mise run docker-build` builds for the
# host's own architecture.

FROM rust:1.92-slim-bookworm AS builder
ARG TARGETARCH

WORKDIR /build
# async-native-tls (IMAP) pulls in native-tls -> openssl-sys on Linux,
# which needs pkg-config + OpenSSL dev headers to build -- neither is in
# the base rust image. CLI-0148 removed `minio`, which used to be named
# here too; this is still required for the IMAP half alone, so neither the
# apt-get below nor the runtime libssl3 may be dropped with it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
# tests/ is not built, but Cargo.toml names `tests/cli.rs` as an explicit
# [[test]] target, and cargo will not load a manifest whose target is missing.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
# CLI-0163: --locked, so a Cargo.lock that has drifted from Cargo.toml fails
# the build rather than silently resolving something new. The cache mounts
# keep the registry and target/ across local rebuilds (per architecture, so
# an arm64 and an amd64 build never share a target/); the binary is copied out
# because a cache mount is not part of the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry-${TARGETARCH} \
    --mount=type=cache,target=/build/target,id=cargo-target-${TARGETARCH} \
    cargo build --release --locked \
    && cp target/release/pigeon /usr/local/bin/pigeon

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
# rclone names its Linux archives by the same arch strings BuildKit uses for
# TARGETARCH (`amd64`, `arm64`), so it is used verbatim.
ARG TARGETARCH
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
    archive="rclone-v${PIGEON_RCLONE_VERSION}-linux-${TARGETARCH}.zip"; \
    base="https://downloads.rclone.org/v${PIGEON_RCLONE_VERSION}"; \
    curl -fsSL -o "$archive" "$base/$archive"; \
    curl -fsSL -o SHA256SUMS "$base/SHA256SUMS"; \
    grep " \*\?$archive\$" SHA256SUMS > expected.sha256; \
    test -s expected.sha256; \
    sha256sum -c expected.sha256; \
    unzip -j "$archive" "*/rclone" -d /rclone/bin; \
    chmod +x /rclone/bin/rclone; \
    /rclone/bin/rclone version

# CLI-0163: a pinned static ffmpeg rather than Debian's. bookworm ships 5.1,
# which predates HEIF "Tile Grid" stream groups -- what `probe_dimensions`
# reads to see a grid-tiled HEIC as one photo rather than one 512x512 tile
# (CLI-0121) -- and the ffmpeg CLI only reconstructs such a grid from 8.1.
# This is the same BtbN build runway's compute-instance-config pins for the
# same reason, so the container and the instances transcode identically.
#
# BtbN publishes no SHA256SUMS that outlives its autobuild release, so each
# arch's digest is pinned here. Bumping the version means bumping all three.
FROM debian:bookworm-slim AS ffmpeg
ARG TARGETARCH
ARG PIGEON_FFMPEG_RELEASE=autobuild-2026-10-10-13-04
ARG PIGEON_FFMPEG_BUILD=ffmpeg-n9.0.2-25-g67b60c310b
ARG PIGEON_FFMPEG_SHA256_AMD64=7e898ca0a18e8620c0caa9af9a728bc01be006398dd5db5a99d53f14de010a9a
ARG PIGEON_FFMPEG_SHA256_ARM64=8f897563d9a39eb5616ab043f29f9018358d1f8c15d4b0d30df0dba71f3a931f
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /ffmpeg
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) arch=linux64; sha256="$PIGEON_FFMPEG_SHA256_AMD64" ;; \
      arm64) arch=linuxarm64; sha256="$PIGEON_FFMPEG_SHA256_ARM64" ;; \
      *) echo "no pinned ffmpeg for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    archive="${PIGEON_FFMPEG_BUILD}-${arch}-gpl-9.0.tar.xz"; \
    curl -fsSL -o "$archive" \
      "https://github.com/BtbN/FFmpeg-Builds/releases/download/${PIGEON_FFMPEG_RELEASE}/${archive}"; \
    echo "${sha256}  ${archive}" | sha256sum -c -; \
    mkdir -p /ffmpeg/bin; \
    tar -xJf "$archive" --strip-components=2 -C /ffmpeg/bin \
      "${archive%.tar.xz}/bin/ffmpeg" "${archive%.tar.xz}/bin/ffprobe"; \
    /ffmpeg/bin/ffmpeg -version; \
    /ffmpeg/bin/ffprobe -version

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
# CLI-0163: `--no-install-recommends` leaves LibreOffice with no text fonts,
# so a docx/pptx rendered to PDF substituted every glyph. Liberation is
# metric-compatible with Arial/Times/Courier, which keeps line breaks where
# the author put them; DejaVu covers what Liberation lacks. `tini` is PID 1,
# so the soffice/ffmpeg/gs children pigeon spawns are reaped and a
# `docker stop` reaches pigeon as a signal it can act on.
#
# `rclone` and `ffmpeg` come from the pinned builder stages above
# (CLI-0155, CLI-0163), not from apt.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       libreoffice-calc libreoffice-writer libreoffice-impress \
       fonts-liberation2 fonts-dejavu-core \
       calibre pandoc \
       ghostscript \
       tini \
       ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /usr/sbin/nologin pigeon
COPY --from=builder /usr/local/bin/pigeon /usr/local/bin/pigeon
COPY --from=rclone /rclone/bin/rclone /usr/local/bin/rclone
COPY --from=ffmpeg /ffmpeg/bin/ffmpeg /ffmpeg/bin/ffprobe /usr/local/bin/

ENV PIGEON_CONFIG_DIR=/data/config
ENV PIGEON_LOG_DIR=/data/logs
VOLUME /data
RUN mkdir -p /data && chown pigeon:pigeon /data

USER pigeon
WORKDIR /data
ENTRYPOINT ["/usr/bin/tini", "--", "pigeon"]
