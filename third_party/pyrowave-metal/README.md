# PyroWave Metal encoder (vendored)

Encoder half of the Metal port of [PyroWave](https://github.com/Themaister/pyrowave)
(MIT, see `LICENSE`), used by `umac-host` for the PyroWave codec.

- Upstream commit: `7fe0e2667cd3076b8b2bf04bcb80bc8d691b800b` (statically audited; see
  `docs/pyrowave.md`).
- Taken from upstream `metal/`: `pyrowave_metal.h`, `pyrowave_common.hpp`,
  `pyrowave_common.mm`, `pyrowave_encoder.mm`, `pyrowave_bitstream.hpp`,
  `pyrowave_bitstream.cpp`, and `shaders/pyrowave_msl.h` (generated MSL, compiled by Metal at
  runtime).
- Not taken: the decoder (`pyrowave_decoder.mm`; umac decodes with its own Rust
  implementation), the Xcode project, CMake files, the export list, Granite and every script.
- Built by `crates/host/build.rs` with the `cc` crate (C++17 / Objective-C++ with ARC); nothing
  from upstream runs at build time.

## Local changes

1. `pyrowave_encoder.mm`, CPU input validation: `row_stride_in_bytes * plane_height` is computed
   with `__builtin_mul_overflow` instead of an unchecked multiply, so a huge stride cannot wrap
   and pass the plane size check (audit finding; umac itself only uses the IOSurface path).

Everything else is byte for byte upstream.
