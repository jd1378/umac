#!/usr/bin/env bash
# Builds umac-host on the Mac as the build user and installs it for the GUI user through
# the idempotent setup wizard (keeps the stable signature, so TCC grants survive).
#
# Usage: UMAC_MAC=<host> UMAC_BUILD_USER=<user> UMAC_GUI_USER=<user> scripts/deploy-host.sh
#   UMAC_MAC         Mac hostname or address
#   UMAC_BUILD_USER  account on the Mac with a Rust toolchain (builds there)
#   UMAC_GUI_USER    account logged in on the Mac's screen (runs the agent)
#   UMAC_SSH_KEY     optional private key; otherwise your ssh config and agent decide
set -euo pipefail
: "${UMAC_MAC:?set UMAC_MAC to the Mac hostname or address}"
: "${UMAC_BUILD_USER:?set UMAC_BUILD_USER to the Mac account that builds}"
: "${UMAC_GUI_USER:?set UMAC_GUI_USER to the Mac account logged in on screen}"
host=$UMAC_MAC
build_user=$UMAC_BUILD_USER
gui_user=$UMAC_GUI_USER
ssh_opts=()
if [[ -n ${UMAC_SSH_KEY:-} ]]; then
    ssh_opts=(-i "$UMAC_SSH_KEY" -o IdentitiesOnly=yes)
fi
root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

rsync -az --delete --exclude target --exclude .git -e "ssh ${ssh_opts[*]}" "$root/" "$build_user@$host:unified_mac/"
ssh "${ssh_opts[@]}" "$build_user@$host" 'cd unified_mac && ~/.cargo/bin/cargo build --locked --release -q -p umac-host && ~/.cargo/bin/cargo test --locked -q -p umac-host 2>&1 | grep -E "test result|FAILED"; ~/.cargo/bin/cargo clippy -q -p umac-host --all-targets 2>&1 | grep -E "^(warning|error)" -A4 || true'
scp -q "${ssh_opts[@]}" "$build_user@$host:unified_mac/target/release/umac-host" "$tmp/umac-host"
ssh "${ssh_opts[@]}" "$gui_user@$host" 'mkdir -p ~/.umac-staging && chmod 700 ~/.umac-staging'
scp -q "${ssh_opts[@]}" "$tmp/umac-host" "$gui_user@$host:.umac-staging/umac-host"
ssh "${ssh_opts[@]}" "$gui_user@$host" 'chmod 700 ~/.umac-staging/umac-host && ~/.umac-staging/umac-host setup </dev/null | grep -E "agent running|ok: Screen|error|Error" || true'
