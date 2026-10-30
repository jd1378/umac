# PyroWave integration

PyroWave is the primary codec; HEVC (VideoToolbox + ffmpeg/VA-API) remains the fallback until
the H.264 Vulkan Video path replaces it. Bitstream: upstream spec v1 (frozen 2026-10-03),
upstream commit `7fe0e2667cd3076b8b2bf04bcb80bc8d691b800b` (audited, see `pyrowave.md`).

## Host (macOS)

- The upstream Metal encoder is vendored in `third_party/pyrowave-metal/` (MIT, upstream
  LICENSE kept) with local fixes listed in its README. Built by `crates/host/build.rs` with the
  `cc` crate; no Granite, no CMake, no build scripts from upstream.
- Input: ScreenCaptureKit NV12 IOSurfaces (4:2:0). 4:4:4 later via a BGRA to planar pass.
- Budget: bytes per frame = width * height * bits_per_pixel / 8. Default 1.2 bpp at 4:2:0
  (about 150 Mbit/s at 1080p60, where small text was visually lossless in the M1 benchmark).
  Lowered on sustained loss, raised back slowly.
- Packet boundary: the largest PyroWave packet that fits one QUIC datagram after the
  22 byte `PyroHeader`. An encoder packet that still exceeds it (a single block larger than a
  datagram, seen at about 1.2 bpp) is split over consecutive datagrams flagged
  `pyro_flags::CONTINUED`; the receiver joins them before parsing. Losing any piece loses
  that packet, like losing an unsplit packet.

## Wire

One datagram = `PyroHeader` (`crates/proto/src/video.rs`) + one encoder packet, or one piece
of it (`CONTINUED`). `packet_count` counts datagrams. No FEC and no
keyframes: every packet decodes on its own. Frames are only sent when the window changed.

## Changed regions only

Blocks absent from a frame keep their previous coefficients on the receiver. The host encodes
the whole frame, then sends only the blocks whose synthesized area touches a changed region
(`crates/pyrowave-decode/src/select.rs`). Changed regions come from ScreenCaptureKit's dirty
rects (read from the sample attachment, CGRect dictionaries) when they cover less than half
the frame; window captures report every frame as fully dirty, so for those the host compares
the frame with the previous one in 16x16 tiles (exact: unchanged pixels are byte identical,
`host/src/pyro.rs::changed_tiles`). A level `l` block
covers `32 * 2^(l+1)` pixels per side, grown by `4 * (2^(l+1) - 1)` pixels of CDF 9/7
synthesis spread plus one coefficient of slack. Kept blocks the encoder left out (quantized
to zero) are sent as empty blocks, which zero them. A rolling refresh resends every block at
least once per 30 changing frames, so a lost block heals; the first frame of an encoder, the
idle refine frame and forced keyframes send every block. The start of frame header announces
the number of blocks actually sent. Host setting `pyrowave_changed_only` (default true) turns
this off for comparisons.

## Client

- `crates/pyrowave-decode`: a Rust reimplementation of the decoder. The bitstream parser is the
  validator (bounds on every header field, payload sizes checked against what the shaders read,
  duplicate and out of range blocks rejected, per frame byte cap), so the upstream parser bugs
  cannot occur. Dequantization and inverse wavelet run as wgpu compute passes writing the
  planes the renderer samples (zero copy).
- A frame is decoded when all `packet_count` packets arrived, when a newer frame starts, or
  after a short timeout; missing blocks keep their previous coefficients.
- The Linux client advertises PyroWave first when the decoder initialized; the host picks the
  first codec both support.
