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
