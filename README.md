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
