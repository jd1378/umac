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
  22 byte `PyroHeader`. Encoder packets that still exceed it (a single oversized block) are
  dropped and counted; the block decodes as blur.

## Wire

One datagram = `PyroHeader` (`crates/proto/src/video.rs`) + one encoder packet. No FEC and no
keyframes: every packet decodes on its own. Frames are only sent when the window changed.

## Client

- `crates/pyrowave-decode`: a Rust reimplementation of the decoder. The bitstream parser is the
  validator (bounds on every header field, payload sizes checked against what the shaders read,
  duplicate and out of range blocks rejected, per frame byte cap), so the upstream parser bugs
  cannot occur. Dequantization and inverse wavelet run as wgpu compute passes writing the
  planes the renderer samples (zero copy).
- A frame is decoded when all `packet_count` packets arrived, when a newer frame starts, or
  after a short timeout; missing blocks decode as zero coefficients (local blur).
- The Linux client advertises PyroWave first when the decoder initialized; the host picks the
  first codec both support.
