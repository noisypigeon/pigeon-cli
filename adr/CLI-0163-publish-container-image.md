# CLI-0163: Publish the container image on every release, for arm64 and amd64

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-20.
- **Status**: Accepted. Implemented on 2026-10-20 (see Verification). Amends
  CLI-0087 and CLI-0155.

## Context

The `Dockerfile` bundles every binary a job shells out to: `rclone`
(CLI-0155), `ffmpeg`, LibreOffice, Calibre, pandoc and Ghostscript (CLI-0154,
CLI-0160). Nothing builds it except `mise run docker-build` on someone's
machine, so no image exists for any released `vX.Y.Z`. Reviewing it turned up
problems beyond the missing publish step:

- **Its ffmpeg is too old for the HEIC work it is there for.** bookworm ships
  ffmpeg 5.1. HEIF "Tile Grid" stream groups, which `probe_dimensions` reads so
  that a grid-tiled HEIC is seen as one photo rather than one 512×512 tile
  (CLI-0121), arrived later, and the ffmpeg CLI only reconstructs such a grid
  from 8.1. runway's `compute-instance-config` already pins a static ffmpeg
  9.0.2 for exactly this reason. So the instances and the container have been
  transcoding HEIC differently.
- **arm64 only** (CLI-0087 §2), including the rclone archive name CLI-0155
  hard-coded.
- **No init process.** `pigeon` is PID 1 and spawns soffice, ffmpeg and gs.
  Nothing reaps an orphaned grandchild, and PID 1 gets no default signal
  handling.
- **No fonts.** `--no-install-recommends` leaves LibreOffice with no text
  fonts, so a docx or pptx rendered to PDF has every glyph substituted.
- **No `--locked` and no build cache.** A drifted `Cargo.lock` would resolve
  silently, and every rebuild compiled every crate from scratch.
- **Stale README guidance.** The Docker section still describes CLI-0087's
  two-phase OS-keyring workflow, which CLI-0135 retired: secrets live in
  `keyring.toml` now.

## Decision

### 1. Publishing is the library's, the Dockerfile stays here

Following the hub and spoke split (LIB-0001, CLI-0161), the mechanics of
building and publishing are the library's new reusable
`container-image-release.yml` (LIB-0019, `workflows/v3.4.0`).

`release.yml` gains an `image` job that `needs: release` and calls that
workflow with:

- the version just released
- `image: ghcr.io/noisypigeon/pigeon-cli`
- a smoke test

The job builds the released tag, not `main`. It builds each architecture on
a native runner, runs the smoke test inside each image before pushing, and
publishes `X.Y.Z`, `X.Y` and `latest` as one multi-arch index with a
build-provenance attestation. `/.lib-version` moves to `workflows/v3.4.0`
with the `uses:` refs, as CLI-0161 requires.

The smoke test is one line per external binary: `pigeon`, `rclone`,
`ffmpeg`, `ffprobe`, `soffice`, `ebook-convert`, `pandoc` and `gs`. A further
line checks that `ffprobe` knows `show_stream_groups`, which is what pins the
HEIC capability rather than merely "some ffmpeg".

### 2. arm64 and amd64, keyed off `TARGETARCH`

This reverses CLI-0087's arm64-only decision. Every stage reads BuildKit's
`TARGETARCH`:

- rclone's archive names use the same `amd64`/`arm64` strings, so its URL takes
  it verbatim.
- The ffmpeg stage maps it to BtbN's `linux64`/`linuxarm64` and refuses any
  other architecture.

`mise run docker-build`/`docker-run` drop `--platform linux/arm64` and build
for the host. A new `docker-pull` task fetches the published image and tags
it `pigeon-cli`, so `docker-run` works on either.

### 3. A pinned, checksum-verified static ffmpeg

A new builder stage fetches the same BtbN `n9.0.2` build runway pins, verifies
it against a per-architecture SHA-256 pinned in the Dockerfile, and copies
only `ffmpeg` and `ffprobe` into the runtime image. This is CLI-0155's rclone
pattern, for CLI-0155's reasons: the version where behaviour is verified
should be the version that runs. The apt `ffmpeg` package is removed.

The amd64 digest was cross-checked against the one runway already pins.
BtbN publishes no `SHA256SUMS` that outlives its release, so the digests live
in build arguments (`PIGEON_FFMPEG_*`, not `FFMPEG_*`, for CLI-0155's
naming reason).

### 4. `tini`, fonts, `--locked`, cache mounts, an allowlisted context

- **`tini` is the entrypoint** (`/usr/bin/tini -- pigeon`).
- **Fonts.** `fonts-liberation2` is metric-compatible with
  Arial/Times/Courier, so line breaks stay where the author put them.
  `fonts-dejavu-core` covers the rest.
- **The build.** `cargo build --release --locked`, with BuildKit cache mounts
  for the registry and `target/`, keyed per architecture.
- **`.dockerignore` becomes an allowlist** (`Cargo.toml`, `Cargo.lock`,
  `src/`, `tests/`). `tests/` stays because `Cargo.toml` names
  `tests/cli.rs` as an explicit `[[test]]` target, and cargo will not load the
  manifest without it.

### 5. README describes the CLI-0135 model

`keyring.toml`, secrets included, lives at `/data/config/keyring.toml` and the
generated `rclone.conf` beside it. So the volume is secret material, and the
README says so. `PIGEON_SECRET_<ALIAS>` is documented as what it now is: an
optional per-run override, not a required second phase. This corrects
CLI-0087 §1's "Resulting non-interactive workflow" in the README; the ADR
itself stays as written.

## Consequences

- Every release publishes `ghcr.io/noisypigeon/pigeon-cli` for both
  architectures, built from the release tag. An image that is missing any
  converter never publishes.
- **The first publish creates a private package.** Making it public is a
  one-time manual step in the package settings on GitHub.
- The container's HEIC transcodes now match the instances'.
- **BtbN prunes old autobuild releases.** Once `autobuild-2026-10-10-13-04`
  is gone, the build fails loudly (`curl -f`) until the three `PIGEON_FFMPEG_*`
  arguments are bumped together. runway's cloud-init has the same exposure.
- **The release run gets longer by the image job.** A native Rust build plus
  the apt layer runs on each architecture. The GHA layer cache only helps when
  the apt layer is unchanged, since the Rust build layer changes with every
  release.

## Out of scope

- **Running runway's instances from this image** instead of installing
  converters through cloud-init. A runway/LIB decision, once something
  deployed needs a job again.
- **Moving the base to Debian trixie** for a newer LibreOffice (its
  `libetonyek` matters for `.numbers`, CLI-0149's unverified fidelity). Rust,
  LibreOffice and Calibre all change at once, which needs its own
  verification pass.

## Verification

Run locally on an Apple Silicon host, 2026-10-20:

- **The full arm64 image builds.** `docker buildx build --platform
  linux/arm64` produced a 2.64 GB image. The smoke test's commands, run inside
  it, report:
  - `pigeon 0.4.56`
  - `rclone v1.75.2`
  - ffmpeg/ffprobe `n9.0.2-25-g67b60c310b`, with `show_stream_groups` present
  - LibreOffice 7.4.7.2
  - calibre 6.13.0
  - pandoc 2.17.1.1
  - Ghostscript 10.00.0

  `ffmpeg` resolves to `/usr/local/bin/ffmpeg`, so no apt copy shadows it.
  `fc-list` lists 12 Liberation faces. `docker run pigeon-cli:… --version`
  works through the `tini` entrypoint.
- **The stream-group check is load-bearing.** bookworm's own
  `ffprobe 5.1.9` lacks `show_stream_groups`, so the smoke test line fails
  against the package this ADR removes.
- **A wrong pinned digest fails the build.** Passing an all-zero
  `PIGEON_FFMPEG_SHA256_ARM64` fails the ffmpeg stage at `sha256sum -c`.
- **amd64's architecture switches work.** The `rclone` and `ffmpeg` stages
  built for `linux/amd64` under emulation fetched, verified and ran rclone
  1.75.2 and ffmpeg n9.0.2. The full amd64 image was **not** built locally,
  because a Rust release build under QEMU is what the native-runner design
  avoids. Its first full build is the release run's `image` job.
- `mise run ci` is clean, and `actionlint` passes on `release.yml`.

**Not verified here:** the published index, its tags and its attestation.
Those exist only once the release workflow runs after merge.
