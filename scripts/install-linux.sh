#!/usr/bin/env bash
# Installs the umac client on Linux into ~/.local/bin/umac.
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/jd1378/umac/main/scripts/install-linux.sh)"
#
# By default it downloads the latest release for this machine (x86_64 or aarch64), checks it
# against the release's SHA256SUMS and, if the GitHub CLI is installed, its build provenance
# attestation. Asks before anything that needs sudo. Re-running updates.
#
# Optional:
#   UMAC_VERSION=v0.2.0   install that release instead of the latest
#   UMAC_FROM_SOURCE=1    build from source instead (UMAC_FEATURES=h264,hevc,mjpeg adds codecs
#                         to the default PyroWave; UMAC_REF, UMAC_REPO, UMAC_SRC pick the source)
#   UMAC_BIN              install directory (default ~/.local/bin)
set -euo pipefail

github=${UMAC_GITHUB:-https://github.com/jd1378/umac}
repo=${UMAC_REPO:-$github.git}
ref=${UMAC_REF:-main}
src=${UMAC_SRC:-${XDG_DATA_HOME:-$HOME/.local/share}/umac/src}
bin=${UMAC_BIN:-$HOME/.local/bin}
features=${UMAC_FEATURES:-}

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
ask() {
    [[ -t 0 ]] || return 1
    read -r -p "$1 [y/N] " reply
    [[ $reply == [yY] || $reply == [yY][eE][sS] ]]
}

[[ $(uname -s) == Linux ]] || die "this installs the Linux client"
[[ -n ${WAYLAND_DISPLAY:-} ]] || printf 'warning: no Wayland session detected; the tray daemon needs Wayland\n'

# Installs missing distribution packages, asking first. Arguments: dnf, apt and pacman lists.
install_packages() {
    local -a dnf=($1) apt=($2) pacman=($3) missing=()
    if command -v dnf >/dev/null 2>&1; then
        for p in "${dnf[@]}"; do rpm -q "$p" >/dev/null 2>&1 || missing+=("$p"); done
        ((${#missing[@]} == 0)) || { ask "Install ${missing[*]} with sudo dnf?" && sudo dnf install -y "${missing[@]}"; } || true
    elif command -v apt-get >/dev/null 2>&1; then
        for p in "${apt[@]}"; do dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p"); done
        ((${#missing[@]} == 0)) || { ask "Install ${missing[*]} with sudo apt-get?" && sudo apt-get install -y "${missing[@]}"; } || true
    elif command -v pacman >/dev/null 2>&1; then
        for p in "${pacman[@]}"; do pacman -Qi "$p" >/dev/null 2>&1 || missing+=("$p"); done
        ((${#missing[@]} == 0)) || { ask "Install ${missing[*]} with sudo pacman?" && sudo pacman -S --needed "${missing[@]}"; } || true
    else
        printf 'unknown distribution: install these yourself: %s\n' "${apt[*]}"
    fi
}

install_release() {
    local arch
    case $(uname -m) in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "no release build for $(uname -m); build from source with UMAC_FROM_SOURCE=1" ;;
    esac

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
    local asset="umac-$version-linux-$arch.tar.gz"

    say "Downloading $asset"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "cannot download $base/$asset"
    curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "cannot download $base/SHA256SUMS"
    grep -E "^[0-9a-f]{64}  $asset\$" "$tmp/SHA256SUMS" > "$tmp/expected" || die "$asset is not listed in SHA256SUMS"
    (cd "$tmp" && sha256sum --quiet -c expected) || die "checksum mismatch for $asset"
    printf 'checksum ok\n'
    if command -v gh >/dev/null 2>&1; then
        gh attestation verify "$tmp/$asset" --repo jd1378/umac >/dev/null || die "build provenance verification failed for $asset"
        printf 'build provenance ok (GitHub attestation)\n'
    else
        printf 'note: install the GitHub CLI (gh) to also verify the build provenance attestation\n'
    fi

    tar -xzf "$tmp/$asset" -C "$tmp"
    install -Dm755 "$tmp/${asset%.tar.gz}/umac" "$bin/umac"
    printf 'installed %s (%s)\n' "$bin/umac" "$tag"

    if ! { ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null; } | grep -c 'libxkbcommon\.so\.0' >/dev/null; then
        printf '\numac needs libxkbcommon at runtime.\n'
        install_packages "libxkbcommon" "libxkbcommon0" "libxkbcommon"
    fi
}

install_source() {
    say "Checking build packages"
    local dnf="libxkbcommon-devel wayland-devel clang git pkgconf-pkg-config"
    local apt="libxkbcommon-dev libwayland-dev clang pkg-config git"
    local pacman="libxkbcommon wayland clang pkgconf git"
    if [[ ,$features, == *,hevc,* ]]; then
        if command -v dnf >/dev/null 2>&1 && ! rpm -q ffmpeg-devel >/dev/null 2>&1 && ! dnf repolist 2>/dev/null | grep -qi rpmfusion; then
            printf 'ffmpeg-devel comes from RPM Fusion (https://rpmfusion.org); enable it first.\n'
        fi
        dnf+=" ffmpeg-devel"
        apt+=" libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev libva-dev"
        pacman+=" ffmpeg libva"
    fi
    install_packages "$dnf" "$apt" "$pacman"

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

    say "Building umac (a few minutes the first time)"
    (cd "$src" && cargo build --locked --release -p umac-client ${features:+--features "$features"})
    install -Dm755 "${CARGO_TARGET_DIR:-$src/target}/release/umac" "$bin/umac"
    printf 'installed %s\n' "$bin/umac"
}

if [[ ${UMAC_FROM_SOURCE:-} == 1 ]]; then
    install_source
else
    install_release
fi
case ":$PATH:" in *":$bin:"*) ;; *) printf 'note: add %s to your PATH\n' "$bin" ;; esac

if [[ $(sysctl -n net.core.rmem_max 2>/dev/null || echo 0) -lt 8388608 ]]; then
    printf '\nLarger UDP buffers keep keyframe bursts from being dropped.\n'
    if ask "Set net.core.rmem_max=16777216 now and at boot (sudo)?"; then
        echo 'net.core.rmem_max = 16777216' | sudo tee /etc/sysctl.d/90-umac.conf >/dev/null
        sudo sysctl -q -p /etc/sysctl.d/90-umac.conf
    fi
fi

if pgrep -u "$USER" -x umac >/dev/null 2>&1; then
    printf '\nA umac daemon is running; restart it to use the new build: umac quit && umac\n'
fi
say "Done. Pair with the link printed on the Mac:  umac pair 'umac://...'"
