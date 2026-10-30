# umac

Streams individual macOS windows to a Linux PC, each as its own native window, with
hardware HEVC encode (VideoToolbox) and decode (VA-API) over a pinned-certificate QUIC link.

- `crates/proto`: wire types.
- `crates/transport`: QUIC endpoints, cert pinning, framing, FEC video datagrams.
- `crates/host`: `umac-host` for the Mac (capture, encode, input, setup wizard).
- `crates/client`: `umac` for Linux (decode, one window per remote window, input).

## Setup

On the Mac, as the user logged in on its screen:

    umac-host setup

Follow the prompts (grant Screen Recording and Accessibility via Screen Sharing), then copy
the printed `umac://...` pairing string. On the PC:

    umac pair 'umac://...'
    umac list
    umac open --app Safari

The Linux client needs `libxkbcommon` at build and run time (Fedora: `libxkbcommon-devel`)
and ffmpeg with VA-API (`ffmpeg-devel`, `mesa-va-drivers-freeworld`). On Wayland it uses
native popups for Mac menus; `umac open --backend winit` selects the fallback shell.

Linux tip: raise `net.core.rmem_max` to 8 MiB or more so keyframe bursts are not dropped.

## Daemon and tray

One `umac daemon` process per user owns every stream window, the tray and the connection to
each Mac. `umac` on its own starts it in the background when a Mac is paired (pairing starts
it too). `umac open`, `launch`, `desktop`, `list` and `displays` talk to it over a private
socket (`$XDG_RUNTIME_DIR/umac/daemon.sock`, same user only) and start it when needed, so a
Mac window is never shown twice: asking for one that is already open brings it to the front.
`umac status` lists the windows shown, `umac quit` stops the daemon and closes them all.
The daemon needs a Wayland session; `--no-daemon` on `open`, `launch`, `desktop`, `list`
and `displays` runs that one command in its own process instead (debugging, X11), ending
with its last window.

Left click on the tray opens Apps; the right-click menu has Apps, Open a terminal window,
Show Desktop, Show task manager, Show settings (Mac System Settings), Paired hosts and Exit, which closes every window and stops
the daemon. Failures show as desktop notifications. On macOS 27 the Apps launcher is an
overlay that cannot be streamed, so Apps shows a Finder window on /Applications instead.

Closing a window here closes it on the Mac too, with its close button, so an unsaved
document asks first (the prompt opens here as a dialog) and the window goes away once the
Mac closed it. `"close_on_mac": false` only stops streaming instead. Popups and desktops
are only stopped.

Paired hosts lists every paired Mac as `name (address)`; the checked one is what the tray and
any command without `--host` use (`"selected_host"` in config.json). The icon's line color
follows the KDE color scheme (`"tray_icon_color": "auto" | "black" | "white"`).

    umac autostart enable    # start the tray at login (~/.config/autostart/umac.desktop)
    umac autostart status
    umac autostart disable

The tray uses the StatusNotifierItem D-Bus protocol (KDE, and GNOME with the AppIndicator
extension).

## New windows and dialogs

When a streamed app opens a window by itself it opens on the PC too (`"follow_new_windows":
true`, default). New documents become their own windows; dialogs, sheets, file panels and
settings windows stay above the window they belong to. Closing one of these on the PC only
stops streaming it; dismiss it on the Mac side (its buttons, Escape) to close it there.

The window the Mac focuses opens on the PC too when it is not shown yet (`"follow_focus":
true`, default): an app you start from a streamed window (double-clicking it in Finder), a
window you switch to with Cmd+Tab, or a password prompt or alert that pops up by itself.
Windows of apps already shown follow the dialog rules above; others open as their own
windows. Closing such a window on the PC keeps it closed while it stays focused on the Mac;
it opens again once focus moves away and comes back. Overlays, the menu bar, the Dock and
the window that was focused when you connected are left alone.

## Clipboard

Text only, up to about 900 KB.

- Mac to PC: every copy made on the Mac lands on the PC clipboard. Wayland lets a client set
  the clipboard only while it has keyboard focus, so a copy that arrives while no umac window
  is focused is applied when one gets focus. Disable with `"clipboard_to_pc": false`.
- PC to Mac: `"clipboard_to_mac"` is `"on_paste"` by default. The PC clipboard is sent only
  when you paste into a Mac window: Cmd+V (Ctrl+V with `swap_ctrl_cmd`) or a Paste item in a
  Mac menu. `"always"` also sends it whenever a umac window gains focus; `"off"` never sends it.

Both live in `~/.config/umac/config.json`, for example:

    { "clipboard_to_pc": true, "clipboard_to_mac": "on_paste" }

Clipboard sync needs the Wayland backend; `--backend winit` does not sync.

## Keyboard

Keys are sent by position, so the Mac's own keyboard layout applies. Mouse wheel notches
scroll `"scroll_lines"` lines each (default 3), times your desktop's scroll speed; touchpads
scroll smoothly in pixels. Held keys repeat on
the Mac at its own repeat rate (System Settings > Keyboard).

With `"swap_ctrl_cmd": true` (default) Ctrl acts as Cmd and Super as Control. With
`"linux_shortcuts": true` (default) Linux text editing habits are translated:

| Linux | Mac |
|---|---|
| Ctrl+Backspace / Ctrl+Delete | Option+Delete / Option+Forward Delete (delete word); in terminals Ctrl+W / Esc D |
| Ctrl+Left / Ctrl+Right (with Shift) | Option+Left / Option+Right (move or select by word) |
| Home / End (with Shift) | Cmd+Left / Cmd+Right (line start / end) |
| Ctrl+Home / Ctrl+End | Cmd+Up / Cmd+Down (document start / end) |

In Mac terminals (Terminal, iTerm2, kitty, Alacritty, WezTerm, Ghostty, Warp) Ctrl stays
Control so Ctrl+C interrupts, Ctrl+Shift+C / Ctrl+Shift+V copy and paste, and Home / End
send Ctrl+A / Ctrl+E.

## Latency

`umac latency [--seconds 10]` streams the main display for a while and prints a per-stage
table; `--stats` on `open`, `desktop` and `launch` (or `"show_stats": true`) logs the same
stages every 2 s. Ctrl+Alt+Shift+S toggles a live readout (glass-to-glass p50 and fps) in the
window titles; that S is not sent to the Mac.

Glass to glass runs from the Mac display refresh a frame belongs to (ScreenCaptureKit's
display time) to when the compositor reports it on screen (Wayland presentation-time; the
winit backend uses the submit time). The host and client clocks are synced with probes over
the control stream; the table prints the remaining uncertainty (half the best round trip).
To measure with steady frames, move the Mac pointer over a desktop stream, e.g.
`cargo run -p umac-host --example wiggle -- 15` in parallel.

## Dependencies

Every dependency is pinned to an exact version (`=x.y.z`) and `Cargo.lock` is committed, so
nothing updates by accident. Build with `--locked`. To update a crate, change its pin
deliberately, run `cargo update -p <crate>`, and review the lockfile diff.
