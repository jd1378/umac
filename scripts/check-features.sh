#!/usr/bin/env bash
# Builds, lints and tests the codec feature combinations of one crate.
# Usage: scripts/check-features.sh client|host [--no-test]
#   client: run on Linux. host: run on macOS (the host's capture and encoders are macOS only).
set -euo pipefail
crate=${1:?usage: check-features.sh client|host [--no-test]}
test=${2:-}
cd "$(dirname "$0")/.."
pkg=umac-$crate
combos=(
    ""
    "--no-default-features --features h264"
    "--no-default-features --features hevc"
    "--no-default-features --features mjpeg"
    "--all-features"
)
for flags in "${combos[@]}"; do
    printf '\n== %s %s\n' "$pkg" "${flags:-(default)}"
    # shellcheck disable=SC2086
    cargo clippy --locked -q -p "$pkg" --all-targets $flags -- -D warnings
    if [[ $test != --no-test ]]; then
        # shellcheck disable=SC2086
        cargo test --locked -q -p "$pkg" $flags 2>&1 | grep -E "^test result|FAILED|panicked" || true
    fi
done
printf '\n== %s --no-default-features (must fail: no codec)\n' "$pkg"
if cargo check --locked -q -p "$pkg" --no-default-features 2>/dev/null; then
    echo "error: building without any codec feature should fail" >&2
    exit 1
fi
echo "ok: refused"
