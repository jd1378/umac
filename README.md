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

Keys are sent by position, so the Mac's own keyboard layout applies. Held keys repeat on
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
