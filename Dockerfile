# syntax=docker/dockerfile:1
# ADR-0087: Dockerizes pigeon-cli. linux/arm64 only -- see the ADR for why.
# Build/run with `docker build/run --platform linux/arm64` (see the `mise
# run docker-build`/`docker-run` tasks); platform is intentionally not
# pinned per-stage here so it's set once, from the command line.

FROM rust:1.92-slim-bookworm AS builder

WORKDIR /build
# async-native-tls/minio pull in native-tls -> openssl-sys on Linux, which
# needs pkg-config + OpenSSL dev headers to build -- neither is in the base
# rust image.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates libssl3 \
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
