#!/usr/bin/env bash
# Installs the umac client on Linux: system packages, Rust, a build from source, and
# ~/.local/bin/umac.
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/jd1378/umac/main/scripts/install-linux.sh)"
#
# Asks before anything that needs sudo. Re-running updates the source and the binary.
# Optional: UMAC_REF (branch, tag or commit, default main), UMAC_REPO, UMAC_SRC, UMAC_BIN.
set -euo pipefail

repo=${UMAC_REPO:-https://github.com/jd1378/umac.git}
ref=${UMAC_REF:-main}
src=${UMAC_SRC:-${XDG_DATA_HOME:-$HOME/.local/share}/umac/src}
bin=${UMAC_BIN:-$HOME/.local/bin}

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
ask() {
    [[ -t 0 ]] || return 1
    read -r -p "$1 [y/N] " reply
    [[ $reply == [yY] || $reply == [yY][eE][sS] ]]
}

[[ $(uname -s) == Linux ]] || die "this installs the Linux client"
[[ -n ${WAYLAND_DISPLAY:-} ]] || printf 'warning: no Wayland session detected; the tray daemon needs Wayland\n'

say "Checking system packages"
if command -v dnf >/dev/null 2>&1; then
    pkgs=(ffmpeg-devel libxkbcommon-devel clang git pkgconf-pkg-config)
    if ! rpm -q ffmpeg-devel >/dev/null 2>&1 && ! dnf repolist 2>/dev/null | grep -qi rpmfusion; then
        printf 'ffmpeg-devel comes from RPM Fusion (https://rpmfusion.org); enable it first.\n'
    fi
    if ! rpm -q mesa-va-drivers-freeworld >/dev/null 2>&1; then
        printf 'note: AMD/Intel hardware HEVC decoding needs mesa-va-drivers-freeworld (RPM Fusion).\n'
    fi
    missing=()
    for p in "${pkgs[@]}"; do rpm -q "$p" >/dev/null 2>&1 || missing+=("$p"); done
    if ((${#missing[@]})); then
        ask "Install ${missing[*]} with sudo dnf?" && sudo dnf install -y "${missing[@]}"
    fi
elif command -v apt-get >/dev/null 2>&1; then
    pkgs=(libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev
        libva-dev libxkbcommon-dev clang pkg-config git)
    missing=()
    for p in "${pkgs[@]}"; do dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p"); done
    if ((${#missing[@]})); then
        ask "Install ${missing[*]} with sudo apt-get?" && sudo apt-get install -y "${missing[@]}"
    fi
elif command -v pacman >/dev/null 2>&1; then
    pkgs=(ffmpeg libva libxkbcommon clang pkgconf git)
    missing=()
    for p in "${pkgs[@]}"; do pacman -Qi "$p" >/dev/null 2>&1 || missing+=("$p"); done
    if ((${#missing[@]})); then
        ask "Install ${missing[*]} with sudo pacman?" && sudo pacman -S --needed "${missing[@]}"
    fi
else
    printf 'unknown distribution: install ffmpeg (with headers), libva, libxkbcommon, clang, pkg-config and git yourself\n'
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

say "Building umac (a few minutes the first time)"
(cd "$src" && cargo build --locked --release -p umac-client)
install -Dm755 "${CARGO_TARGET_DIR:-$src/target}/release/umac" "$bin/umac"
printf 'installed %s\n' "$bin/umac"
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
