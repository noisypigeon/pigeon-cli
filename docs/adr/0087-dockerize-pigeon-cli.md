# ADR-0087: Dockerize pigeon-cli

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-01.
- **Status**: Accepted.

## Context

`pigeon` currently requires a local Rust 1.92 toolchain to build, a macOS ad-hoc codesign step for Keychain ACL stability (ADR-0016), and — for `job run pull-transform` — `ffmpeg`/`ffprobe` on `PATH` (ADR-0074). This ADR adds a Docker image as an alternative way to run `pigeon` that bundles all of that, so a user doesn't need any of it installed locally. Infrastructure (orchestration, deployment, secret managers) is explicitly out of scope here — the user is handling that separately. This ADR covers only the image itself and the one code change needed to make it actually usable non-interactively.

### The credential blocker

Every secret `pigeon` reads goes through `credentials::get_secret` (`src/core/keyring/credentials.rs:21-27`) — confirmed the single choke point for every job wizard's secret lookups (email-sync, email-pull, dedupe, sort, pull-transform, decrypt-files all call it directly). That function talks to the OS-native credential store via the `keyring` crate. On Linux, ADR-0085 made that the kernel-keyutils backend: in-memory only, explicitly scoped to "ad hoc/interactive, not unattended cron" use, and not persisted across a process/session reset.

A Docker container restart is a harder reset than the reboot case ADR-0085 already accepted as a trade-off. With no existing escape hatch — there is no env-var or file-based credential injection anywhere in this codebase today — every container restart would otherwise force re-running `pigeon keyring add` interactively before any job could read a secret. That's acceptable for a single long-lived interactive container session, but not for a container meant to be restarted or run one-shot, which is the normal shape of a containerized tool.

Rather than just documenting that limitation, this ADR adds a minimal non-interactive credential path to `pigeon` itself, orthogonal to ADR-0085's OS-keyring fix.

## Decision

### 1. `PIGEON_SECRET_<ALIAS>` environment variable fallback

`get_secret` checks an environment variable before it touches the OS keyring:

```rust
pub fn get_secret(alias: &str) -> Result<String, String> {
    if let Ok(secret) = std::env::var(env_var_name(alias)) {
        return Ok(secret);
    }
    // existing keyring::Entry path, unchanged
}
```

`env_var_name(alias)` is `PIGEON_SECRET_` followed by `alias` uppercased with every character outside `[A-Z0-9_]` replaced by `_`. Aliases are already required to be globally unique across kinds (ADR-0022), but two different aliases could in principle sanitize to the same env var name (e.g. `gmail-work` and `gmail_work`); avoiding that collision is the caller's responsibility, not something this fallback guards against.

`set_secret` and `delete_secret` are **unchanged** — they still only read/write the OS keyring. This is deliberately asymmetric: the env var is a read-side override for consumption, not a new place `keyring add`/`modify` can write to. The naming follows the existing `PIGEON_CONFIG_DIR` (`src/commands/keyring/store.rs:15`) / `PIGEON_LOG_DIR` precedent.

This fallback is OS-agnostic, not container-specific — it works identically on macOS/Windows/Linux, and is equally usable outside Docker (e.g. scripting, testing). It's introduced here because it's what makes the container usable non-interactively at all, but it isn't a container-only mechanism.

#### Resulting non-interactive workflow

No new CLI surface is added. The existing commands compose into a two-phase pattern:

1. **One-time setup**: run `pigeon keyring add <kind> <alias>` once (interactively, or piped — `pigeon` already reads a plain line from stdin when it isn't a TTY) to populate `keyring.toml`'s non-secret metadata (alias, provider, host, bucket endpoint, etc.). `keyring.toml` holds no secret material, so it can be persisted in a mounted volume and reused across container runs via `$PIGEON_CONFIG_DIR`.
2. **Per-run**: supply the real secret value via a `PIGEON_SECRET_<ALIAS>` environment variable at container start, sourced from whatever secret manager the surrounding infrastructure provides. `get_secret` returns it directly and never touches the OS keyring on that path — so a freshly started container with an empty kernel keyring works correctly.

### 2. Dockerfile (repo root)

A multi-stage build:

- **Builder stage**: `rust:1.92-slim-bookworm`, matching `.mise.toml`'s pinned `rust = "1.92.0"`. Installs `pkg-config`/`libssl-dev` — `async-native-tls` and `minio` (via `reqwest`/`hyper-tls`) both pull in `native-tls` → `openssl-sys` on Linux, which needs both to build and isn't present in the base image. Copies the manifest and source (the same files `Cargo.toml`'s `include` allowlist already names: `src/`, `tests/`, `Cargo.toml`, `Cargo.lock`), then `cargo build --release`. No codesign step — that's `.mise.toml`'s `build`/`pigeon` tasks' macOS-only branch and doesn't apply on Linux.
- **Runtime stage**: `debian:bookworm-slim`. `apt-get install -y --no-install-recommends ffmpeg ca-certificates libssl3` — `ffmpeg` bundles `ffprobe` (both required by `pull-transform`'s `check_ffmpeg_available()` preflight check), and `libssl3` is the shared OpenSSL runtime the dynamically-linked `native-tls` path needs. Creates a non-root user to run as. Copies the built `pigeon` binary from the builder stage into `/usr/local/bin`.
- `ENV PIGEON_CONFIG_DIR=/data/config` and `ENV PIGEON_LOG_DIR=/data/logs`; `VOLUME /data` is the single mount point covering config, logs, and job local-output/staging directories.
- `ENTRYPOINT ["pigeon"]`, no default `CMD`, so a bare `docker run` surfaces `pigeon`'s own `--help`.
- Debian slim (glibc) was chosen over an Alpine/musl base: `ffmpeg` needs an OS package manager either way, and this project's dependencies (`keyring`'s zbus/keyutils backends, `async-native-tls`, `aes-gcm-siv`, etc.) have no validated musl build history here — glibc is the lower-risk choice.
- The image targets **`linux/arm64` only**; build/run instructions pass `--platform linux/arm64` explicitly rather than setting up a multi-arch `buildx` matrix. The `FROM` lines themselves don't pin a platform (BuildKit would otherwise warn about a constant `--platform` value there) — the `docker build --platform`/`docker run --platform` flags set it once, for the whole build/run.

### 3. `.dockerignore` (repo root)

Excludes `target/`, `docs/`, `.claude/`, `.git/`, and `CLAUDE.md` from the build context, mirroring the same packaging-scope logic `Cargo.toml`'s `include` list already applies (ADR-0050/ADR-0086) so ADRs and Claude tooling don't bloat the build context or image layer cache.

### 4. New `mise` tasks

- `mise run docker-build` → `docker build --platform linux/arm64 -t pigeon-cli .`
- `mise run docker-run` → `docker run --platform linux/arm64 --rm -it -v pigeon-data:/data pigeon-cli "$@"`

This keeps Docker usage discoverable the same way every other task in CLAUDE.md's Commands section already is.

### 5. README.md

Adds a "Run via Docker" section covering the build/run commands, the `/data` volume convention, and the two-phase non-interactive credential workflow from §1.

## Consequences

- `pigeon` can be built and run entirely via Docker on `linux/arm64`, with no local Rust toolchain or `ffmpeg` install required.
- A container can now be used non-interactively (one-shot or restarted) for jobs needing secrets, by combining a persisted `keyring.toml` (via a mounted `$PIGEON_CONFIG_DIR`) with per-run `PIGEON_SECRET_<ALIAS>` env vars — without relying on the Linux kernel keyring surviving a restart.
- The `PIGEON_SECRET_<ALIAS>` fallback is available on every platform, not just inside Docker; it's a small, generally useful addition to `get_secret`, not a container-specific hack.
- `set_secret`/`delete_secret` behavior, and the OS-keyring path generally (including ADR-0085's Linux kernel-keyutils backend), are completely unchanged — this is a new read-side fallback checked first, not a replacement.
- No amd64 image is published; an Apple Silicon (arm64) host is assumed for both building and running the image.
- Deployment, orchestration, and secret-manager wiring (e.g. how `PIGEON_SECRET_*` env vars actually get populated at container-start time) are left entirely to the surrounding infrastructure, which this ADR does not define.

## Out of scope

- Publishing the image to any container registry (GHCR, Docker Hub).
- Orchestration/deployment tooling — compose, Kubernetes manifests, systemd units, Terraform — all explicitly the user's own infrastructure layer.
- `amd64` image support.
- Any actual secret-manager integration (Vault, Docker secrets, Kubernetes Secrets, etc.) — this ADR only makes `pigeon` able to read a plain environment variable; how that variable gets populated is not addressed here.
- CI wiring for the Docker build — this repo has no `.github/workflows/` at all; `mise run docker-build` stays a manual, locally-gated task like every other `mise run` task in this project.

Implementation lands in the same PR as this ADR.
