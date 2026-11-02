# umac

Use apps from a headless Mac on your Linux desktop. Each Mac window opens as its own native
window on Linux. Video is PyroWave, a low latency wavelet codec encoded on the Mac's GPU and
decoded on the PC's GPU, over an encrypted, certificate-pinned QUIC link on your LAN.

- Separate windows, not a remote desktop: menus, dropdowns, dialogs and sheets appear where
  they belong; new windows and the window the Mac focuses open automatically.
- Whole-desktop mode when you want it (`umac desktop`).
- Clipboard sync, Mac cursor shapes, key repeat, Linux-style shortcuts.
- A tray icon for the common actions; one background process owns every window.
- About 15 ms (p50) from the Mac compositing a frame to your screen, at 1080p over Wi-Fi.

## Requirements

- **Mac:** Apple Silicon, macOS 15 or newer (developed on macOS 27). A display or an HDMI
  dummy plug if it runs headless.
- **PC:** Linux (x86_64 or aarch64) with a Wayland desktop (developed on Fedora with KDE
  Plasma) and a Vulkan GPU.
- Both on the same network; UDP port 47810 must reach the Mac (47811 and up for its other
  users, see [Several Mac users](#several-mac-users)).

## 1. Install on the Mac

On the Mac, as the user logged in on its screen (over Screen Sharing, or SSH as that user):

```sh
bash -c "$(curl -fsSL https://raw.githubusercontent.com/jd1378/umac/main/scripts/install-mac.sh)"
```

[scripts/install-mac.sh](scripts/install-mac.sh) (read it first if you like):

1. downloads the latest release, checks it against the release's `SHA256SUMS` and, if the
   GitHub CLI (`gh`) is installed, its build provenance attestation,
2. runs `umac-host setup`, which installs the agent as a LaunchAgent and signs it locally so
   macOS keeps its permissions across updates.

`UMAC_VERSION=v1.0.0` installs a specific release. `UMAC_FROM_SOURCE=1` builds from source
instead (Command Line Tools and Rust, offered if missing).

The wizard asks you to allow **Screen & System Audio Recording** and **Accessibility** for
`umac-host` in System Settings > Privacy & Security (do this over Screen Sharing), and checks
auto-login and sleep settings. At the end it prints a pairing link like
`umac://192.0.2.10:47810?fp=...&token=...`, valid for 10 minutes and usable once.

Run the same command again to update. `umac-host pair` prints a new pairing link,
`umac-host status` shows the agent's state, `umac-host uninstall` removes it.

## 2. Install on Linux

```sh
bash -c "$(curl -fsSL https://raw.githubusercontent.com/jd1378/umac/main/scripts/install-linux.sh)"
```

[scripts/install-linux.sh](scripts/install-linux.sh) downloads the latest release for your
machine (x86_64 or aarch64), verifies it like the Mac script, installs `~/.local/bin/umac`, and
offers to raise `net.core.rmem_max` so keyframe bursts are not dropped. The only runtime
library it needs is libxkbcommon. It asks before anything that needs sudo. Run it again to
update; `UMAC_VERSION` picks a release.

Building from source instead: `UMAC_FROM_SOURCE=1` (installs build packages and Rust, asking
first), or by hand with `cargo build --locked --release -p umac-client`.

### Codecs

Every codec is a cargo feature; release builds contain only PyroWave. Both ends offer what they
were built with and use the first codec they share, in this order:

| Feature | Codec | Mac | PC | Notes |
|---|---|---|---|---|
| `pyrowave` (default) | PyroWave | Metal compute (vendored encoder) | wgpu compute | Lowest latency, sharpest; about 25 to 200 Mbit/s |
| `h264` | H.264 | VideoToolbox | Vulkan Video | Fallback for low bandwidth; the driver must expose H.264 decode (Fedora: RPM Fusion's `mesa-vulkan-drivers-freeworld`) |
| `hevc` | HEVC (and H.264) | VideoToolbox | ffmpeg with VA-API | Needs ffmpeg development files to build (Fedora: RPM Fusion) |
| `mjpeg` | Motion-JPEG | VideoToolbox | zune-jpeg on the CPU | Last resort |

Add codecs with `--features h264,mjpeg` (or `UMAC_FEATURES=h264,mjpeg UMAC_FROM_SOURCE=1` in the
install scripts). If the two ends share no codec, the Mac refuses the connection and names the
features that would fix it. `UMAC_CODECS=h264` limits what a client offers, for testing.

## 3. Pair

```sh
umac pair 'umac://192.0.2.10:47810?fp=...&token=...'
```

The link pins the Mac's certificate fingerprint, so the client only talks to that Mac, and the
Mac only accepts clients it paired. Pairing starts the tray.

`umac hosts` lists paired hosts with a short id; `--host <id>` picks one for a command and
`umac unpair <id>` forgets one (`umac-host unpair` on the Mac makes the Mac forget this PC).

### Several Mac users

Every Mac user who should be streamed runs setup (and pairs) once, as that user. Each user's
agent listens on its own UDP port: setup keeps the user's saved port while it is free, else
takes 47810 plus the user's offset from uid 501, or the next free port, and saves it in
`~/Library/Application Support/umac/settings.json` (`"port"`). A user who is logged in but
switched out (fast user switching) can run setup over SSH; permissions are granted later,
while that user is on screen.

A user can be streamed while their desktop is on screen: at the Mac itself, or in a Screen
Sharing session logged in as that user (macOS gives it a display of its own, so one user can
be at the Mac while another is streamed). The tray treats a Mac's users as one Mac: "Mac user"
picks whom actions go to, falling back to another paired user who is on screen. When a user
leaves the screen, their windows close here and a notification says so. umac cannot switch
users or log them in itself: do that at the Mac or with Screen Sharing.

## Use

- `umac` starts the tray if it is not running. Left click: Apps. Right click: Apps, Open a
  terminal window, Show Desktop, Show task manager, Show settings, Paired hosts, Exit.
- `umac autostart enable` starts it at login.
- `umac list` shows the Mac's windows; `umac open --app Safari` or `--window <id>` opens them
  here. `umac launch terminal|finder|activity-monitor|settings` opens something new.
- `umac desktop` shows a whole Mac display (F11 for fullscreen).
- `umac status`, `umac close --window <id>`, `umac quit`.
- `umac hosts`, `umac unpair <id>`.

Closing a window here closes it on the Mac too (an unsaved document asks first).
Ctrl+Alt+Shift+S shows live latency in window titles; `umac latency` measures it.

### Keyboard

Ctrl acts as Cmd, so Ctrl+C, Ctrl+V and Ctrl+T work as on Linux. Linux text editing keys are
translated (Ctrl+Backspace, Ctrl+Arrows, Home/End). In Mac terminals Ctrl stays Control
(Ctrl+C interrupts) and Ctrl+Shift+C / Ctrl+Shift+V copy and paste. KDE keeps some
combinations for itself (Super, Alt+Tab).

### Clipboard

Copies on the Mac land on the Linux clipboard. Your Linux clipboard goes to the Mac only when
you paste into a Mac window (Cmd+V or a Paste menu item), never in the background.

## Settings

`~/.config/umac/config.json` (every key optional):

| Key | Default | Meaning |
|---|---|---|
| `bitrate_kbps` | 25000 | Video bitrate per window (desktops scale with resolution) |
| `max_fps` | 60 | Frame rate cap |
| `swap_ctrl_cmd` | true | Ctrl acts as Cmd, Super as Control |
| `linux_shortcuts` | true | Translate Linux editing keys and terminal keys |
| `scroll_lines` | 3 | Lines per mouse wheel notch (times your desktop scroll speed) |
| `clipboard_to_pc` | true | Mac copies reach the Linux clipboard |
| `clipboard_to_mac` | `on_paste` | `off`, `on_paste` or `always` (on window focus) |
| `follow_new_windows` | true | Open dialogs and new windows of streamed apps |
| `follow_focus` | true | Open the window the Mac focuses (launched apps, alerts) |
| `close_on_mac` | true | Closing a window here closes it on the Mac |
| `tray_icon_color` | `auto` | `auto`, `black` or `white` |
| `show_stats` | false | Log per-window latency stages |
| `view_scale` | 1 | Pixels per Mac point: 1 shows Mac windows at their natural size even when the Mac runs Retina or 4K; up to 2 makes them larger and sharper |

## Security

- Every connection is QUIC (TLS 1.3) with pinned self-signed certificates on both sides; a
  client must pair with a one-time token before the Mac accepts it.
- The Mac agent has Screen Recording and Accessibility, so a paired client can see and control
  the Mac. Pair only your own machines; `umac-host unpair <fingerprint>` removes one.
- Launch actions are a fixed list (Terminal, Finder, Activity Monitor, System Settings);
  clients cannot run commands on the Mac.
- The client daemon's control socket is only reachable by your user.
- Every dependency is pinned to an exact version and `Cargo.lock` is committed; builds use
  `--locked`. [deny.toml](deny.toml) documents the accepted advisories.

## Troubleshooting

- **Black or tiny windows after a Mac reboot:** a headless Mac may fall back to a low
  resolution; use a display or an HDMI dummy plug.
- **Nothing captured:** run `umac-host status` on the Mac; re-run the install command to
  re-check permissions.
- **Logs:** Mac `~/Library/Logs/umac/agent.log`, Linux `~/.local/state/umac/daemon.log`.

## Development

Crates: `proto` (wire types), `transport` (QUIC, pinning, FEC video datagrams), `host`
(`umac-host`, macOS), `client` (`umac`, Linux).

`scripts/deploy-host.sh` builds and installs the host on a Mac over SSH:

    UMAC_MAC=mac.local UMAC_BUILD_USER=builder UMAC_GUI_USER=me scripts/deploy-host.sh

`UMAC_SSH_KEY` optionally selects a private key. `scripts/gen-tray-masks.sh` regenerates the
tray icon masks from `crates/client/assets/tray.svg` (needs Inkscape and ImageMagick).
PyroWave integration notes: [docs/pyrowave-integration.md](docs/pyrowave-integration.md).

`scripts/check-features.sh client` (on Linux) and `scripts/check-features.sh host` (on macOS)
build, lint and test every codec feature combination. The Rust version is pinned in
`rust-toolchain.toml`.

### CI and releases

`.github/workflows/ci.yml` lints and tests every push: the client on Linux x86_64 and aarch64
(default features on Ubuntu 22.04, all features in a Fedora container for RPM Fusion's ffmpeg)
and the host on macOS (default and all features).

Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`: it builds `umac-host` for macOS
arm64 and `umac` for Linux x86_64 and aarch64 (default features, Ubuntu 22.04 so any glibc 2.35+
distribution runs it), attests each archive's build provenance, and publishes them with
`SHA256SUMS` as a GitHub release. Running the workflow by hand only builds. Verify a download
yourself with `gh attestation verify <archive> --repo jd1378/umac`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

Apple, Mac, macOS, ScreenCaptureKit and VideoToolbox are trademarks of Apple Inc. umac is an
independent project, not affiliated with or endorsed by Apple. The tray icon is original
artwork (a window with the Command key symbol) under the same license as the code.
