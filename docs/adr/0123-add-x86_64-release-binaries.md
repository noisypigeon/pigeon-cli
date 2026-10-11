# ADR-0123: add x86_64 release binaries (native Linux, cross-compiled macOS)

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-11.
- **Status**: Proposed.

## Context

ADR-0117 introduced `.github/workflows/release.yml`'s `build-binaries`
job: a 2-leg matrix building native arm64 binaries only (`macos-14` →
`aarch64-apple-darwin`, `ubuntu-24.04-arm` →
`aarch64-unknown-linux-gnu`), deliberately justified by picking GitHub's
native arm64-hosted runners so that "no cross-compilation or
Docker-image extraction" was needed at all. That same ADR explicitly
scoped x86_64 out for either platform: "nothing in this repo's existing
tooling (`docker-build`, the dev machine) currently targets them."

That gap is real today: a user on an Intel Mac or an x86_64 Linux box
has no precompiled `pigeon` binary option — only `cargo install
pigeon-cli`, compiling from source on their own machine. This ADR
reverses ADR-0117's x86_64 out-of-scope bullet and adds both missing
targets.

The two new targets are not symmetric in difficulty:

- **Linux x86_64** is trivially native. `ubuntu-24.04`-hosted GitHub
  runners are x86_64 by default, so this leg is a straight copy of the
  existing Linux arm64 leg's shape — same pattern, just a different
  runner label and triple.
- **macOS x86_64** requires cross-compilation, which is new for this
  repo. GitHub's macOS hosted-runner fleet is arm64-only now —
  `macos-13` was the last Intel-hosted image, and `macos-14`/`macos-15`
  are both Apple-Silicon-hosted. There is no native-host option left for
  this target, unlike when ADR-0117 was written. Fortunately this is an
  unusually low-risk cross-compile: Xcode's own `clang`/`ld` toolchain
  links for either Apple Silicon or Intel directly from an Apple Silicon
  host (`rustup target add x86_64-apple-darwin` followed by `cargo build
  --release --target x86_64-apple-darwin`) — no Docker, no QEMU, no
  foreign sysroot to wrangle. This is not the class of cross-compilation
  that usually needs guesswork.

One real, accepted limitation falls out of the macOS case: the
cross-compiled `x86_64-apple-darwin` binary can be build-verified in CI
but never run-verified — GitHub's hosted fleet has no x86_64 macOS
execution environment to invoke it against. The other three legs
(including the new Linux x86_64 one) all run natively on their own build
runner, so they *can* be smoke-tested — and since `build-binaries`
currently has zero execution verification for any leg, this ADR adds a
`pigeon --version` smoke test for the three legs that can run it, rather
than deferring that gap to a separate ADR.

## Decision

### Hoist `build-release` into a script, add `--target`

`.mise.toml`'s `build-release` task becomes a one-line passthrough to a
new script, matching the existing `release`/`adr-issue` precedent (mise
only threads a real `"$@"` through a single-line task body ending in
`"$@"` — see ADR-0117's own note on this):

```toml
[tasks.build-release]
run = ".github/scripts/build-release.sh \"$@\""
```

New `.github/scripts/build-release.sh`, matching `release.sh`'s/
`cut-release.sh`'s conventions (`set -euo pipefail`, a `usage()`
function, long-flag parsing):

```bash
#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: build-release.sh [--target <triple>]

  --target   Rust target triple to build for, e.g. x86_64-apple-darwin.
             Installs the target via `rustup target add` first; binary
             lands at target/<triple>/release/pigeon. When omitted, runs
             a plain `cargo build --release` for the host's native
             target, landing at target/release/pigeon.
EOF
}

target=""
while [ $# -gt 0 ]; do
  case "$1" in
    --target) target="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 1 ;;
  esac
done

if [ -n "$target" ]; then
  rustup target add "$target"
  cargo build --release --target "$target"
  bin_path="target/${target}/release/pigeon"
else
  cargo build --release
  bin_path="target/release/pigeon"
fi

if [ "$(uname)" = "Darwin" ]; then
  codesign --force -s - --identifier "dev.pigeon.cli" "$bin_path"
fi
```

With no `--target`, this is byte-for-byte the same behavior
`build-release` already had — local `mise run build-release` and `mise
run pigeon-release` (which keeps its own inline `cargo build --release`,
unaffected by this change) are unchanged.

### `release.yml`: 4-leg matrix, uniform `--target`, smoke test

```yaml
  build-binaries:
    needs: release
    strategy:
      matrix:
        include:
          - runner: macos-14
            triple: aarch64-apple-darwin
            smoke_test: true
          - runner: ubuntu-24.04-arm
            triple: aarch64-unknown-linux-gnu
            smoke_test: true
          - runner: macos-14
            triple: x86_64-apple-darwin
            smoke_test: false
          - runner: ubuntu-24.04
            triple: x86_64-unknown-linux-gnu
            smoke_test: true
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4
        with:
          ref: v${{ needs.release.outputs.new_version }}

      - uses: jdx/mise-action@v2

      - run: mise run build-release -- --target ${{ matrix.triple }}

      - name: Smoke test
        if: matrix.smoke_test
        run: target/${{ matrix.triple }}/release/pigeon --version

      - name: Package binary
        run: |
          version="${{ needs.release.outputs.new_version }}"
          triple="${{ matrix.triple }}"
          tar czf "pigeon-v${version}-${triple}.tar.gz" -C "target/${triple}/release" pigeon -C "$GITHUB_WORKSPACE" LICENSE.md

      - name: Upload release asset
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          version="${{ needs.release.outputs.new_version }}"
          triple="${{ matrix.triple }}"
          gh release upload "v${version}" "pigeon-v${version}-${triple}.tar.gz"
```

`--target` is passed uniformly on all four legs, including the two that
already ran natively. `rustup target add` for a triple that's already
the host's default is a cheap no-op, and paying that lets every leg
share the exact same `target/<triple>/release/pigeon` path in the
smoke-test and packaging steps — no branching on "is this a cross leg or
not." The only observable change to the two pre-existing legs is where
`build-release.sh` writes its output on disk; nothing outside these two
steps reads that path.

The macOS x86_64 leg reuses the `macos-14` runner label — the same
arm64-hosted image as the existing macOS leg — since there is no
Intel-hosted macOS image to target instead; this is the cross-compile
host. The new Linux leg uses `ubuntu-24.04` (pinned, not `ubuntu-latest`)
to match the existing `ubuntu-24.04-arm` leg's explicit-version
convention.

## Consequences

- Every release now carries 4 precompiled binaries instead of 2:
  `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`,
  `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`.
- `build-binaries`'s total CI compute roughly doubles, but stays
  matrix-parallel. The two macOS legs share the same runner *label*
  (`macos-14`); if GitHub's account-level concurrency ever queues
  same-type runners, that pair could serialize rather than run
  simultaneously — a possible, not guaranteed, small latency increase,
  not a blocker.
- The macOS x86_64 leg is build-verified only. It is explicitly never
  run-verified in CI — there is no x86_64 macOS execution environment in
  GitHub's hosted fleet to invoke the cross-compiled binary against,
  unlike the other three legs, which all run natively on their own
  build runner's architecture. A runtime-only defect in that one binary
  (as opposed to a build-time one) could ship undetected until a real
  Intel Mac user hits it. Accepted: there is currently no CI-available
  way to close that gap, and Apple's own toolchain cross-linking from
  arm64 to x86_64 is low-risk relative to, say, cross-OS compilation.
- The three natively-executable legs now also smoke-test their binary
  with `pigeon --version` before packaging — a real, if narrow,
  execution check that `build-binaries` never had for any leg before
  this ADR.

## Out of scope

- macOS x86_64 notarization or Developer-ID signing — the same
  accepted limitation ADR-0117 already noted for the arm64 macOS
  binary (ad-hoc codesign only), now shared by this binary too. Still
  requires a paid Apple Developer ID.
- Windows binaries — no existing tooling or dev-machine precedent
  targets Windows at all; a materially larger scope (PE packaging, no
  codesign precedent in this repo) than "add the missing x86_64
  targets."
- musl/statically-linked Linux builds — the new x86_64 Linux leg matches
  the existing arm64 Linux leg's glibc-linked `*-unknown-linux-gnu`
  precedent exactly; musl is an orthogonal portability improvement, not
  implied by this ADR.
- Pinning third-party GitHub Actions (`actions/checkout`,
  `jdx/mise-action`) by commit SHA instead of version tag — already out
  of scope per ADR-0117, unchanged here.

## Verification

- `mise run fmt-check` and `mise run lint` clean on the branch (no Rust
  source changes, so `cargo test` isn't newly exercised by this ADR).
- Locally on the Apple Silicon dev machine: `mise run build-release`
  with no args still produces `target/release/pigeon` exactly as before.
  `mise run build-release -- --target x86_64-apple-darwin` produces
  `target/x86_64-apple-darwin/release/pigeon`; confirm with `file` /
  `lipo -info` that it's a genuine x86_64 Mach-O, not an accidental
  reuse of the host build.
- After merging: watch the next automated release's Actions run, confirm
  all 4 `build-binaries` matrix legs succeed, the 3 smoke-tested legs
  each print a `pigeon --version` line matching the release version, and
  the GitHub Release ends up with all four
  `pigeon-v<version>-<triple>.tar.gz` assets attached.
