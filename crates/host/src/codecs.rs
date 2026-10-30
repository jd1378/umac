//! Codecs compiled into this build, chosen with the cargo features `pyrowave` (default),
//! `h264`, `hevc` and `mjpeg`.

use umac_proto::Codec;

#[cfg(not(any(feature = "pyrowave", feature = "h264", feature = "hevc", feature = "mjpeg")))]
compile_error!("umac-host needs at least one codec feature: pyrowave, h264, hevc or mjpeg");

pub const COMPILED: &[Codec] = &[
    #[cfg(feature = "pyrowave")]
    Codec::PyroWave,
    #[cfg(feature = "h264")]
    Codec::H264,
    #[cfg(feature = "hevc")]
    Codec::Hevc,
    #[cfg(feature = "mjpeg")]
    Codec::Mjpeg,
];

/// The cargo feature that builds `codec`, for error messages.
pub fn feature(codec: Codec) -> &'static str {
    match codec {
        Codec::PyroWave => "pyrowave",
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Mjpeg => "mjpeg",
    }
}

/// Why a client and this build share no codec, naming the features that would fix it.
pub fn no_common(client: &[Codec]) -> String {
    let names = |cs: &[Codec]| cs.iter().map(|&c| feature(c)).collect::<Vec<_>>().join(", ");
    if client.is_empty() {
        return "the client offered no codecs".into();
    }
    format!("no codec in common: this host is built with [{}], the client with [{}]; rebuild one of them with a shared codec feature", names(COMPILED), names(client))
}
