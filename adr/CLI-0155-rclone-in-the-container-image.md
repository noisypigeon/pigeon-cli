# CLI-0155: `rclone` in the container image, from a pinned upstream binary

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-15.
- **Status**: Accepted.

## Context

Every `pigeon job run` requires `rclone` on `PATH`. Since CLI-0132 consolidated
every invocation behind `core::rclone`, and CLI-0133 migrated `deduplicate`'s
last data legs onto it, there is no job left that can run without it --
`transform` pulls and pushes with it, `deduplicate` lists/copies with it,
`email import` delivers with it, `rclone copy`/`delete` *are* it, and CLI-0100's
report upload uses `CopyTo`.

The container image has never installed it. Filed as
[#96](https://github.com/noisypigeon/pigeon-cli/issues/96) when CLI-0149 noticed
it, re-noted by CLI-0154 §11 while adding four more packages to the same layer
and explicitly deferred there rather than widened into that ADR's scope. This
ADR takes it up.

The effect is total, not partial: a container built from this repo today fails
on the first rclone invocation of any job. The binary is also the one
dependency with no in-process fallback -- CLI-0154's `tsv`/`textclipping` kinds
need no converter, but they still need rclone to fetch and deliver.

### Debian's rclone would work, and is still the wrong choice

Bookworm ships **rclone 1.60.1** (November 2022). Upstream is **1.75.2**
(2026-10-09), and the machine this project is developed and tested on runs
**1.72.1**. Compatibility was checked rather than assumed, and apt would in fact
work:

- Every flag `core::rclone` passes predates 1.60 -- `--files-from-raw` (1.52),
  `--use-json-log` (1.53), `--fast-list`, `--tpslimit`,
  `--multi-thread-streams`, `--buffer-size`, `--ignore-case`, `--stats`,
  `--low-level-retries`, `--checkers`, `--transfers`, `--files-only`.
- `LsjsonEntry` deserializes only `Name`, `Path` and `Size`, each
  `#[serde(default)]` -- present since `lsjson` existed.
- `core::rclone::log`'s parser defaults every field and skips an unparseable
  line, so a changed log shape degrades rather than breaks.

So the argument against apt is not "it is broken". It is that a **15-minor-version
skew between the machine where behaviour is verified and the machine where it
runs** is a bad trade for one saved line, in a codebase that has twice shipped a
bug whose entire signature was *something resolved to nothing and exited 0*
(CLI-0139's filter that matched no files, CLI-0140's source address that
resolved to no bucket). A container-only rclone difference is exactly the shape
of defect this project is worst at seeing, and 2022's S3 backend is four years
of fixes away from what every real run has been tested against.

## Decision

### 1. A pinned upstream binary, checksum-verified

A new builder stage fetches a pinned release, verifies it against the project's
published `SHA256SUMS`, extracts it, and **runs it once** as a smoke test
before it is copied into the runtime image.

Verified, not merely downloaded over TLS. `sha256sum -c` against the upstream
`SHA256SUMS` file makes a corrupted or substituted archive a **build failure**
rather than a container that runs an unknown binary -- the same posture
CLI-0150 took on the release path, where a benign-looking no-op was the thing
to avoid.

The fetch happens in the **builder** stage, not the runtime one, so `curl` and
`unzip` do not survive into the shipped image; only the extracted binary is
`COPY`'d across. That keeps the runtime image's installed-package set unchanged
from CLI-0154's.

`linux/arm64` only, consistent with CLI-0087 -- the architecture is already
fixed for this image, so no multi-arch selection logic is needed and none is
added.

### 2. The build arg is **not** called `RCLONE_VERSION`

It is `PIGEON_RCLONE_VERSION`, and the reason is a trap worth recording
because nothing about it is guessable.

rclone configures itself from `RCLONE_<FLAG>` environment variables. A Docker
`ARG RCLONE_VERSION` is visible in the environment of every `RUN` in its stage,
so rclone reads it as the `--version` *boolean* flag and dies before doing
anything:

```text
CRITICAL: Invalid value when setting --version from environment variable
RCLONE_VERSION="1.75.2": invalid argument "1.75.2" for "--version" flag:
strconv.ParseBool: parsing "1.75.2": invalid syntax
```

The obvious name is the one name that cannot be used. Avoiding the
`RCLONE_` prefix entirely kills the whole class rather than this one instance,
which matters because the next person to add an rclone call to this stage
would otherwise rediscover it.

**This was found by §1's smoke test, not by reasoning** -- the download,
checksum and extraction had all succeeded, and the binary on disk was correct.
Only actually running it surfaced the problem, which is the argument for
keeping that line.

### 3. The version is an `ARG`, and bumping it is manual

`--build-arg RCLONE_VERSION=` overrides it for a one-off, and the default moves
when someone deliberately moves it. **This is a real cost and is stated rather
than hidden**: unlike apt, nothing will deliver a security fix on its own, so
the pin is a thing that must be maintained. It is accepted because a pinned,
checksummed, reproducible build is worth more here than unattended patching of
a tool this deployment invokes against its own buckets with its own
credentials.

### 4. `1.75.2`, not `1.72.1`

Pinning to the exact version on the development machine was considered -- it
would make container and dev box byte-identical. Rejected: the point of §1 is
to stop the container drifting *behind*, and starting two releases behind on
day one re-creates a smaller version of the problem. The gap between 1.72.1 and
1.75.2 is one month, against the four years apt would have cost.

## Consequences

- **A container built from this repo can run a job for the first time.** Every
  `pigeon job run` was previously broken in the image; none were.
- The runtime image gains one binary (~70 MB extracted) and no packages.
- `curl` and `unzip` are added to the **builder** only.
- The image build now makes two network fetches it did not before, so a
  build behind a strict egress policy needs `downloads.rclone.org` reachable.
  Stated because the build previously needed only the Debian and crates.io
  mirrors.
- A new maintenance obligation: the pin (§2).
- [#96](https://github.com/noisypigeon/pigeon-cli/issues/96) closes.

## Out of scope

- **Multi-architecture images.** CLI-0087 fixed this image at `linux/arm64`;
  nothing here revisits that, and the single-arch assumption is what lets §1
  hardcode one asset name.
- **Installing `rclone` for non-container use.** A local developer install stays
  the reader's own business, as it was before; this ADR changes only the image.
- **Automating the version bump.** A Dependabot-style updater for a Dockerfile
  `ARG` is a tooling decision of its own, and this repo has no such automation
  for any other pinned thing.
- **Shrinking the image.** It now carries `ffmpeg`, three LibreOffice packages,
  Calibre, pandoc and rclone, and measures **2.38 GB**. That total is large and
  known; CLI-0154 accepted it on the grounds that the alternative is a job that
  cannot run where it is deployed, and this ADR is that argument's final
  instance rather than a new one.
- **Proving a complete job run inside the container.** See Verification: it
  needs real credentials and a reachable S3 endpoint, and the image is not a
  path anything currently depends on. Worth doing if and when that changes.

## Amends

CLI-0087 (its image gains the one dependency every job needs, and its build
grows a second network source) and CLI-0154 (§11's re-noted deferral of #96 is
taken up). Each gets a short forward-pointing "Amendment".

## Verification

Scoped deliberately. The container is not a primary deployment path at the
moment -- jobs are run from the local machine -- so this was verified at the
level of "the image contains and can execute the right binary", not by
standing up credentials and an S3 endpoint inside it.

- `mise run ci` clean (fmt-check + lint + test). No Rust change, so the count
  is unchanged at 564.
- `docker build --platform linux/arm64` succeeds; the resulting image is
  2.38 GB.
- **The checksum step is load-bearing, not decorative.** Proved by appending a
  byte to the downloaded archive mid-build and observing
  `sha256sum: WARNING: 1 computed checksum did NOT match` fail it. A
  nonexistent version separately fails at `curl` (exit 22). Both were run.
- `docker run --entrypoint rclone pigeon-cli version` reports **`rclone
  v1.75.2`** from inside the **shipped** image, not the builder stage.
- All five converters resolve on `PATH` in the shipped image: `rclone`,
  `ffmpeg`, `soffice`, `ebook-convert`, `pandoc`.
- `curl` and `unzip` are **absent** from the shipped image, confirming §1's
  builder-stage confinement actually holds rather than merely being intended.
- §2's env-var collision was found by the smoke test during this work, not by
  review.

**Not verified, and deliberately so**: a full `pigeon job run` executed inside
the container. Every job resolves `--source-bucket`/`--destination-bucket`
through a keyring alias into a generated S3 rclone config (CLI-0136/0140), so
exercising one needs real credentials and a reachable endpoint; a local
directory is not expressible as a bucket-config. That is worth doing when the
image becomes a path something actually depends on, and is not worth
manufacturing now. Recorded here rather than left implied, because the claim
this ADR makes is "the image contains a working, verified rclone" -- not "the
image has been shown to complete a job".
