# umac

Use apps from a headless Mac on your Linux desktop. Each Mac window opens as its own native
window on Linux, with hardware video encoding on the Mac (VideoToolbox HEVC) and hardware
decoding on the PC (VA-API, zero-copy into Vulkan), over an encrypted, certificate-pinned
QUIC link on your LAN.

- Separate windows, not a remote desktop: menus, dropdowns, dialogs and sheets appear where
  they belong; new windows and the window the Mac focuses open automatically.
- Whole-desktop mode when you want it (`umac desktop`).
- Clipboard sync, Mac cursor shapes, key repeat, Linux-style shortcuts.
- A tray icon for the common actions; one background process owns every window.
- About 19 ms (p50) from the Mac compositing a frame to your screen, at 1080p over Wi-Fi.

## Requirements

- **Mac:** Apple Silicon, macOS 15 or newer (developed on macOS 27). A display or an HDMI
  dummy plug if it runs headless.
- **PC:** Linux with a Wayland desktop (developed on Fedora with KDE Plasma) and a GPU with
  VA-API HEVC decoding.
- Both on the same network; UDP port 47810 must reach the Mac.

## 1. Install on the Mac

On the Mac, as the user logged in on its screen (over Screen Sharing, or SSH as that user):

```sh
bash -c "$(curl -fsSL https://raw.githubusercontent.com/jd1378/umac/main/scripts/install-mac.sh)"
```

[scripts/install-mac.sh](scripts/install-mac.sh) (read it first if you like):

1. checks for Apple's Command Line Tools and offers to install Rust (rustup) if missing,
2. downloads this repository and builds `umac-host`,
3. runs `umac-host setup`, which installs the agent as a LaunchAgent and signs it locally so
   macOS keeps its permissions across updates.

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

[scripts/install-linux.sh](scripts/install-linux.sh) asks before anything that needs sudo:

1. installs build dependencies (ffmpeg with headers, libva, libxkbcommon, clang) with dnf,
   apt or pacman; on Fedora, ffmpeg and hardware HEVC decoding come from
   [RPM Fusion](https://rpmfusion.org) (`ffmpeg-devel`, `mesa-va-drivers-freeworld`),
2. offers to install Rust (rustup) if missing,
3. builds `umac` from this repository into `~/.local/bin/umac`,
4. offers to raise `net.core.rmem_max` so keyframe bursts are not dropped.

Run it again to update.

Codecs, in order of preference: PyroWave (GPU compute, no extra libraries), H.264 through
Vulkan Video (needs a driver that exposes it; on Fedora RPM Fusion's
`mesa-vulkan-drivers-freeworld`), then HEVC and H.264 through ffmpeg/VA-API. The ffmpeg path
is the `ffmpeg` cargo feature (on by default); `cargo build -p umac-client
--no-default-features` builds a client without ffmpeg. `UMAC_VULKAN_VIDEO=0` turns the Vulkan
Video path off.

## 3. Pair

```sh
umac pair 'umac://192.0.2.10:47810?fp=...&token=...'
```

The link pins the Mac's certificate fingerprint, so the client only talks to that Mac, and the
Mac only accepts clients it paired. Pairing starts the tray.

## Use

- `umac` starts the tray if it is not running. Left click: Apps. Right click: Apps, Open a
  terminal window, Show Desktop, Show task manager, Show settings, Paired hosts, Exit.
- `umac autostart enable` starts it at login.
- `umac list` shows the Mac's windows; `umac open --app Safari` or `--window <id>` opens them
  here. `umac launch terminal|finder|activity-monitor|settings` opens something new.
- `umac desktop` shows a whole Mac display (F11 for fullscreen).
- `umac status`, `umac close --window <id>`, `umac quit`.

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
PyroWave is reserved as a codec but not integrated; see [docs/pyrowave.md](docs/pyrowave.md).

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
