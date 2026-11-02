# CLI-0165: Run `mise run ci` on every pull request

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)), drafted with Devin.
- **Date**: 2026-11-01.
- **Status**: Accepted. Implemented on 2026-11-01. Depends on runway's
  LIB-0055 (noisypigeon/runway#208, released as `github/workflows/v11.3.0`). Amends CLI-0029, CLI-0148, CLI-0161 and
  CLI-0163. Closes
  [#47](https://github.com/noisypigeon/pigeon-cli/issues/47) and
  [#50](https://github.com/noisypigeon/pigeon-cli/issues/50).

## Context

No test runs anywhere except on the developer's machine. `release.yml`
(a thin caller of runway's `rust-crate-release.yml`, CLI-0161) triggers on
`push` to `main`, runs `fmt-check` and `lint`, and then publishes to
crates.io. It never runs `cargo test`, so a test-breaking commit is
merged, version-bumped and published before anything notices. CLI-0029
chose a local-only gate on purpose, and CLI-0148 recorded the missing PR
check as an accepted gap. CLI-0117's first `release.yml` did run
`mise run ci`, but only after the merge. CLI-0119 cut that back to
fmt-check and lint after the tests failed on the runner. #47 tracks the
gap that's left.

#47 names three blockers. A fresh Ubuntu 22.04 machine (a Devin VM, no
`mise`, plain `cargo`) was used to measure each one:

- **`rclone` missing: 29 of `tests/cli.rs`'s 90 tests fail.** All 29 fail
  on `'rclone' was not found on PATH`. `prepare_rclone`
  (`shared_wizard.rs`) runs before the flag validation those tests assert
  on, so they fail before reaching the error they look for.
- **Distro `rclone`/`ffmpeg` are too old: 6 of 594 lib tests fail.** With
  Ubuntu's `rclone` 1.53 and `ffmpeg` 4.4 installed,
  `purges_everything_under_a_local_directory` and five
  `transform::media` tests (`probe_dimensions`, `apply_cropping`) still
  fail. With the pinned `rclone` 1.75.2 (CLI-0155) and a BtbN `ffmpeg`
  n9.0.2 build (CLI-0163) on `PATH`, **all 594 lib tests and all 90 CLI
  tests pass.** `soffice`, `pandoc` and `gs` are not needed: their tests
  print `skipping: … not found on PATH` and return early.
- **The OS keyring is no longer a blocker.** Since CLI-0135, secrets live
  in `keyring.toml`, and every keyring test passed on the headless VM.

**#50** (`log_file_flag_writes_valid_jsonl_with_the_command_name` came
back with an empty log file on a CI runner) **has a reproducible
mechanism.** The test never sets a log level, so `observability::init`
falls back to the inherited `RUST_LOG`, then to `warn,pigeon=info`. The
`command` span and its `command started`/`finished` events are `info`.
Locally:

| `RUST_LOG` | result |
|---|---|
| unset | pass |
| `info` | pass |
| `warn` | **fails with #50's exact assertion** (`!contents.trim().is_empty()`) |
| `error` | **fails with #50's exact assertion** |

Whether the runner that produced #50 actually had a stricter `RUST_LOG`
is **not verified**: that workflow (CLI-0117's `release.yml`) set none
itself. Either way, the test depends on its environment when it shouldn't.

**One more finding: the image's `ffmpeg` pin no longer resolves.**
CLI-0163 pins `autobuild-2026-10-10-13-04`, and that GitHub release now
returns 404. BtbN keeps about 14 days of daily autobuilds plus each
month's last build (every month-end release back to
`autobuild-2024-12-31-13-02` is still listed). The next `image` job
would fail at its `curl`. It hasn't failed yet only because the last
release run (#146) skipped `image` after two macOS `build-binaries` legs
were cancelled. CI would install the same pin, so this ADR has to fix it.

## Decision

### 1. A thin caller of runway's `rust-crate-ci.yml`

`.github/workflows/ci.yml` triggers on `pull_request` and
`workflow_dispatch`, with `permissions: contents: read` and `concurrency`
keyed on the PR (`cancel-in-progress: true`). Its one job is
`uses: noisypigeon/runway/.github/workflows/rust-crate-ci.yml@github/workflows/v11.3.0`,
which runs this repo's own `mise run ci` unchanged (LIB-0055). So CI and
the local gate cannot drift. This is the CLI-0161 pattern
`release.yml` already follows: the hub owns how CI runs, and this repo
owns the trigger and what the tests need.

### 2. The Dockerfile is the only place tools are pinned

The caller's `setup` snippet exports the Dockerfile's own `rclone` and
`ffmpeg` stages (`docker buildx build --target rclone|ffmpeg --output
type=local,…`) and puts their `bin/` directories on `$GITHUB_PATH`. CI
tests exactly the binaries the image ships, with the same checksum
checks, and nothing parses or copies a pin. Bumping a pin in the
Dockerfile bumps CI too, and the PR that bumps it is the one that tests
it.

### 3. Adopt `.config/` and the current library tracks

LIB-0050 §7 step 6 and LIB-0052 step 5 leave pigeon-cli to adopt the new
layout "with its next real change". The `uses:` ref must equal the pin,
and v11.3.0 sits on the new tracks, so this is that change:

| Now | Becomes |
|---|---|
| `.lib-version` (`workflows/v9.3.0`) | `.config/version` (`github/workflows/v11.3.0`) |
| `fetch-lib.sh`, `LIB_SRC` | `.config/tools.sh` (runway's, verbatim), `PIGEON_SRC` |
| `/.lib-src/` ignore | `/.config/src/` ignore |
| `.lib-src/lib/scripts/` in `.mise.toml` | `.config/src/tools/github/scripts/` |
| `release.yml`'s `uses:` at `workflows/v9.3.0` | `github/workflows/v11.3.0`. The `fetch_script`/`scripts_dir` defaults already name `.config/` |
| `PIGEON_OBSERVABILITY_VERSION=observability/pigeon-cli/v1.0.0`, archiving `lib/observability/pigeon-cli` and `lib/grafana-dashboards/pigeon-cli` | `observability/lgtm-stack/v1.0.1`, archiving `tools/observability/lgtm-stack` and `tools/observability/dashboards/pigeon-cli` (the compose file's `../dashboards/pigeon-cli` mount still resolves) |
| `.claude/settings.json` at `claude-plugins/pigeon-cli/v1.0.1` | `claude/plugins/pigeon-cli/v1.0.2` |

`.mise.toml` stays at the root. LIB-0052 §1's move of it into
`.config/mise.toml` is runway's layout, and step 5 asks pigeon-cli for
the fetch only. CLAUDE.md's "The library" section is rewritten to match.

### 4. `ffmpeg` moves to a month-end autobuild

`PIGEON_FFMPEG_RELEASE=autobuild-2026-10-31-12-59`,
`PIGEON_FFMPEG_BUILD=ffmpeg-n9.0.2-39-g7adfbe40dc`, with that release's
`checksums.sha256` digests:

- amd64 `73e29a56974ae5a651133a04584c7483b21afe941e17dc91d79814a758b608d3`
- arm64 `6cad88ae5bcec356cfc5d7979a036cc4007128201928a0cf243df3db483276cc`

Still n9.0.2, so CLI-0121/0163's stream-group requirement still holds.
The full suite passed against this build. The Dockerfile comment records
the rule: **pin only a month-end autobuild**, because those are the ones
BtbN keeps. This is observed behavior, not a documented promise, and the
comment says so.

### 5. #50: the test pins its own log level

`log_file_flag_writes_valid_jsonl_with_the_command_name` passes
`--log-level info` and `env_remove("RUST_LOG")`, since the flag takes
precedence over `RUST_LOG` in `init`. No production change.

### 6. CLI-0029's merge step waits for the check

Step 3 (local `mise run ci` before `gh pr create`) stays. Step 6 becomes
"merge once the local gate is clean **and** the PR's `CI` check is
green", and CLAUDE.md says the same. Requiring the check through branch
protection is the owner's setting to turn on.

## Consequences

- Every PR gets a visible status check that runs the full suite against
  the image's own `rclone`/`ffmpeg`. A test break no longer reaches
  crates.io unnoticed, as long as merges wait for the check.
- Each PR costs one Linux runner job, a few minutes with `rust-cache`
  (not yet measured).
- pigeon-cli catches up from `workflows/v9.3.0` to the current CI track,
  so `release.yml` and `image` take every hub fix since v9.3.0.
- The next `image` job builds again, instead of failing on a 404.
- The `soffice`/`pandoc`/`gs` document tests still skip on CI, as they do
  on any machine without those tools.
- A commit pushed straight to `main` without a PR still skips the tests.

## Out of scope

- **Requiring the check through branch protection.** An owner setting.
- **Tests inside `release.yml`** before publishing (LIB-0055's Out of
  scope).
- **macOS CI**, and the cancelled macOS `build-binaries` legs.
- **runway's own `ffmpeg` pin** (`compute-instance-config`), which
  probably has the same expiry problem.
- **Moving `.mise.toml` into `.config/`.**
- **Reordering `prepare_rclone` after flag validation**, so those 29
  tests stop needing `rclone`. CI installs `rclone` anyway.

## Amends

- **CLI-0029** §2/§5: the PR check joins the local gate. It does not
  replace it.
- **CLI-0148**: #47 is no longer an accepted gap.
- **CLI-0161**: the fetch moves from `.lib-version`/`fetch-lib.sh`/`.lib-src/`
  to `.config/`, on the `github/workflows/`, `observability/lgtm-stack/`
  and `claude/plugins/` tracks.
- **CLI-0163**: the `ffmpeg` pin and the month-end pinning rule.

## Verification

- **Local, before the PR** (Ubuntu 22.04, `PATH` holding pinned `rclone`
  1.75.2 and BtbN `ffmpeg` n9.0.2-39, `RUST_LOG` unset): `mise run ci`'s
  three steps (`cargo fmt --check`, `clippy -D warnings`, `cargo test`) are
  clean on the branch, with 594/594 lib and 90/90 CLI tests passing.
- **§2's export**: `docker build --target rclone|ffmpeg -o type=local,…`
  (with §4's pin) yields `rclone/bin/rclone` and `ffmpeg/bin/{ffmpeg,ffprobe}`.
- **#50**: with §5 applied, the test passes under `RUST_LOG=warn` and
  `RUST_LOG=error`.
- **The fetch**: `.config/tools.sh` resolves `.config/src @
  github/workflows/v11.3.0` with `crate-*.sh` and `adr-issue.sh` in
  `tools/github/scripts/`. `observability-stack`'s archive yields
  `lgtm-stack/` beside `dashboards/pigeon-cli/`, and `docker compose config`
  accepts it.
- **On the PR**: the `CI` check runs green on a real runner. That is the
  first server-side `cargo test` this repo has had since CLI-0119.
