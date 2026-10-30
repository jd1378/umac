# PyroWave integration status

`Codec::PyroWave` is reserved in the wire protocol but not implemented. The codec is
treated as untrusted until the following are done.

Static audit of upstream commit `7fe0e2667cd3076b8b2bf04bcb80bc8d691b800b` (read only,
nothing built or run). Upstream issue drafts live in `../pyrowave_issues/`.

Findings that block integration:

- HIGH: a 16 byte packet with a duplicate block index and `payload_words = 0` makes
  `pyrowave_decoder_push_packet` loop forever (Vulkan and Metal parsers).
- MEDIUM: the dequant shader derives read offsets from in-block fields without bounding
  them by `payload_words`; crafted blocks read past the payload buffer.
- LOW: per-sequence memory bounded only by block count, state mutated before validation,
  unaligned bitfield casts, `stride * height` overflow on CPU buffer paths.

Conditions before enabling it:

1. Rust-side packet validator in front of the decoder that walks block headers, rejects
   `payload_words < 2`, duplicate or out-of-range block indices, totals that do not match
   the datagram, implied coefficient sizes larger than `payload_words`, and caps bytes per frame.
2. Vulkan device created with `robustBufferAccess` (or robustness2).
3. Decoder on its own thread with a watchdog; packets only from the authenticated QUIC peer
   (already the case for all video).
4. Granite (Vulkan side dependency) vendored at the pinned commit and reviewed.
5. Metal encoder port is "minimal support, may be removed" upstream: plan to own a fork.
6. Bandwidth: 150 to 300 Mbit/s per 1080p60 stream. The Mac is on Wi-Fi 6, so this only
   makes sense for one window at a time or after moving the Mac to Ethernet.
