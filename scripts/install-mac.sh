#!/usr/bin/env bash
# Installs umac-host on this Mac and runs the setup wizard.
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/jd1378/umac/main/scripts/install-mac.sh)"
#
# Run it as the user logged in on the Mac's screen (over Screen Sharing or SSH). By default it
# downloads the latest release, checks it against the release's SHA256SUMS and, if the GitHub
# CLI is installed, its build provenance attestation. Re-running updates; the wizard is safe to
# repeat.
#
# Optional:
#   UMAC_VERSION=v0.2.0   install that release instead of the latest
#   UMAC_FROM_SOURCE=1    build from source instead (needs the Command Line Tools and Rust;
#                         UMAC_FEATURES=h264,hevc,mjpeg adds codecs to the default PyroWave;
#                         UMAC_REF, UMAC_REPO, UMAC_SRC pick the source)
set -euo pipefail

github=${UMAC_GITHUB:-https://github.com/jd1378/umac}
repo=${UMAC_REPO:-$github.git}
ref=${UMAC_REF:-main}
support="$HOME/Library/Application Support/umac"
src=${UMAC_SRC:-$support/src}
features=${UMAC_FEATURES:-}

say() { printf '\n==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
ask() {
    [[ -t 0 ]] || return 1
    read -r -p "$1 [y/N] " reply
    [[ $reply == [yY] || $reply == [yY][eE][sS] ]]
}

[[ $(uname -s) == Darwin ]] || die "this installs the Mac host; run it on the Mac"
[[ $(uname -m) == arm64 || ${UMAC_FROM_SOURCE:-} == 1 ]] \
    || die "release builds are for Apple Silicon; on an Intel Mac build from source with UMAC_FROM_SOURCE=1 (untested)"
console=$(stat -f %Su /dev/console)
[[ $console == "$USER" ]] || die "run this as $console, the user logged in on the Mac's screen (windows can only be captured from that session)"

# Prints the path of the downloaded, verified umac-host. Messages go to stderr.
download_release() {
    local tag=${UMAC_VERSION:-}
    if [[ -z $tag ]]; then
        say "Finding the latest release"
        local url
        url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$github/releases/latest") || die "cannot reach $github"
        tag=${url##*/releases/tag/}
        [[ $url == */releases/tag/* && -n $tag ]] || die "no release has been published yet; build from source with UMAC_FROM_SOURCE=1"
    fi
    local version=${tag#v}
    local base=${UMAC_RELEASE_BASE:-$github/releases/download/$tag}
    local asset="umac-host-$version-macos-arm64.tar.gz"
    local dir="$support/download"

    say "Downloading $asset"
    rm -rf "$dir"
    mkdir -p "$dir"
    chmod 700 "$dir"
    curl -fsSL "$base/$asset" -o "$dir/$asset" || die "cannot download $base/$asset"
    curl -fsSL "$base/SHA256SUMS" -o "$dir/SHA256SUMS" || die "cannot download $base/SHA256SUMS"
    grep -E "^[0-9a-f]{64}  $asset\$" "$dir/SHA256SUMS" > "$dir/expected" || die "$asset is not listed in SHA256SUMS"
    (cd "$dir" && shasum -a 256 -s -c expected) || die "checksum mismatch for $asset"
    printf 'checksum ok\n' >&2
    if command -v gh >/dev/null 2>&1; then
        gh attestation verify "$dir/$asset" --repo jd1378/umac >/dev/null || die "build provenance verification failed for $asset"
        printf 'build provenance ok (GitHub attestation)\n' >&2
    else
        printf 'note: install the GitHub CLI (gh) to also verify the build provenance attestation\n' >&2
    fi
    tar -xzf "$dir/$asset" -C "$dir"
    local exe="$dir/${asset%.tar.gz}/umac-host"
    # Browsers flag downloads with a quarantine attribute; curl does not, but make sure.
    xattr -d com.apple.quarantine "$exe" 2>/dev/null || true
    printf '%s\n' "$exe"
}

# Prints the path of the built umac-host. Messages go to stderr.
build_source() {
    say "Checking Apple's Command Line Tools"
    if ! xcode-select -p >/dev/null 2>&1; then
        xcode-select --install || true
        die "finish installing the Command Line Tools in the dialog on the Mac's screen, then run this again"
    fi

    say "Checking Rust"
    export PATH="$HOME/.cargo/bin:$PATH"
    if ! command -v cargo >/dev/null 2>&1; then
        printf 'Rust is needed to build umac (installed per user by rustup, https://rustup.rs).\n' >&2
        ask "Install Rust now?" || die "install Rust from https://rustup.rs, then run this again"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >&2
    fi

    say "Fetching umac ($ref)"
    if [[ -d $src/.git ]]; then
        git -C "$src" fetch --depth 1 origin "$ref" >&2
        git -C "$src" checkout --quiet --force FETCH_HEAD
    else
        mkdir -p "$(dirname "$src")"
        git clone --quiet --depth 1 --branch "$ref" "$repo" "$src" 2>/dev/null \
            || { git clone --quiet "$repo" "$src" && git -C "$src" checkout --quiet "$ref"; }
    fi
    printf 'building commit %s\n' "$(git -C "$src" rev-parse --short HEAD)" >&2

    say "Building umac-host (a few minutes the first time)"
    (cd "$src" && cargo build --locked --release -p umac-host ${features:+--features "$features"}) >&2
    printf '%s\n' "${CARGO_TARGET_DIR:-$src/target}/release/umac-host"
}

if [[ ${UMAC_FROM_SOURCE:-} == 1 ]]; then
    exe=$(build_source)
else
    exe=$(download_release)
fi

say "Running the setup wizard"
# The wizard copies itself to its install location and signs it there.
"$exe" setup
