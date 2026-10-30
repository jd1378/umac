#!/usr/bin/env bash
# Installs umac-host on this Mac: builds it from source and runs the setup wizard.
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/jd1378/umac/main/scripts/install-mac.sh)"
#
# Run it as the user logged in on the Mac's screen (over Screen Sharing or SSH).
# Re-running updates the source and the installed agent; the wizard is safe to repeat.
# Optional: UMAC_REF (branch, tag or commit to build, default main), UMAC_REPO, UMAC_SRC.
set -euo pipefail

repo=${UMAC_REPO:-https://github.com/jd1378/umac.git}
ref=${UMAC_REF:-main}
src=${UMAC_SRC:-$HOME/Library/Application Support/umac/src}

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
ask() {
    [[ -t 0 ]] || return 1
    read -r -p "$1 [y/N] " reply
    [[ $reply == [yY] || $reply == [yY][eE][sS] ]]
}

[[ $(uname -s) == Darwin ]] || die "this installs the Mac host; run it on the Mac"
[[ $(uname -m) == arm64 ]] || printf 'warning: only Apple Silicon Macs are tested\n'
console=$(stat -f %Su /dev/console)
[[ $console == "$USER" ]] || die "run this as $console, the user logged in on the Mac's screen (windows can only be captured from that session)"

say "Checking Apple's Command Line Tools"
if ! xcode-select -p >/dev/null 2>&1; then
    xcode-select --install || true
    die "finish installing the Command Line Tools in the dialog on the Mac's screen, then run this again"
fi

say "Checking Rust"
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo >/dev/null 2>&1; then
    printf 'Rust is needed to build umac (installed per user by rustup, https://rustup.rs).\n'
    ask "Install Rust now?" || die "install Rust from https://rustup.rs, then run this again"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi

say "Fetching umac ($ref)"
if [[ -d $src/.git ]]; then
    git -C "$src" fetch --depth 1 origin "$ref"
    git -C "$src" checkout --quiet --force FETCH_HEAD
else
    mkdir -p "$(dirname "$src")"
    git clone --quiet --depth 1 --branch "$ref" "$repo" "$src" 2>/dev/null \
        || { git clone --quiet "$repo" "$src" && git -C "$src" checkout --quiet "$ref"; }
fi
printf 'building commit %s\n' "$(git -C "$src" rev-parse --short HEAD)"

say "Building umac-host (a few minutes the first time)"
(cd "$src" && cargo build --locked --release -p umac-host)

say "Running the setup wizard"
"${CARGO_TARGET_DIR:-$src/target}/release/umac-host" setup
