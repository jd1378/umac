//! Wire types shared by the macOS host and the remote client.
//!
//! Control and input travel as length-prefixed postcard frames on reliable QUIC
//! streams. Video travels as unreliable QUIC datagrams (see [`video`]).

pub mod keys;
pub mod video;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible wire change. Peers with different versions refuse to talk.
pub const PROTOCOL_VERSION: u32 = 2;

/// ALPN advertised on the QUIC handshake.
pub const ALPN: &[u8] = b"umac/2";

pub const DEFAULT_PORT: u16 = 47_810;

/// Upper bound for one control/input frame. Cursor images are the largest legit payload.
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// Host-assigned window identifier (the macOS `CGWindowID`).
pub type WindowId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    Hevc,
    H264,
    /// Reserved: intra-only wavelet codec, not implemented yet.
    PyroWave,
}

/// Fixed pixel contract for every codec: 8-bit, BT.709 primaries and matrix, limited range.
/// Kept explicit so future codecs (4:4:4, 10-bit) negotiate instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorFormat {
    Yuv420Bt709Limited,
}

/// First message on the control stream, client to host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub client_name: String,
    /// Codecs the client can decode, most preferred first.
    pub codecs: Vec<Codec>,
    /// One-time token from `umac-host pair`; only needed when this client cert is not yet trusted.
    pub pairing_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HelloReply {
    Accepted { host_name: String },
    Rejected { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub pid: i32,
    pub app_name: String,
    pub bundle_id: Option<String>,
    pub title: String,
    /// Size of the captured surface in pixels.
    pub width: u32,
    pub height: u32,
    /// Backing scale of the window's display (2.0 on retina, 1.0 on a 1080p dummy).
    pub scale: f32,
    /// Window that owns this one (sheets, menus, popovers), if known.
    pub parent: Option<WindowId>,
    /// Top-left of this window relative to the parent's surface origin, in parent surface
    /// pixels. Can be negative or extend past the parent. Zero when there is no parent.
    pub offset: (i32, i32),
    pub kind: WindowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowKind {
    Normal,
    /// Menus, tooltips, popovers: short lived, placed relative to `parent`.
    Popup,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    ListWindows,
    /// Start streaming a window. The host replies with `StreamStarted` or `Error`.
    Subscribe {
        window: WindowId,
        max_fps: u16,
        /// Target bitrate in kbit/s; host clamps to its own limits.
        bitrate_kbps: u32,
    },
    Unsubscribe { window: WindowId },
    /// Resize the Mac window so its captured surface becomes `width`x`height` pixels.
    Resize { window: WindowId, width: u32, height: u32 },
    /// Client window gained focus: bring the Mac window to front so input lands there.
    Focus { window: WindowId },
    /// Decoder lost state (loss, corruption); next frame must be a keyframe.
    RequestKeyframe { window: WindowId },
    Ping { nonce: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMsg {
    Windows(Vec<WindowInfo>),
    /// Reply to `Subscribe`, or sent unsolicited for a popup (menu, tooltip) that opened from
    /// a streamed window; the client shows it relative to `info.parent` until `WindowClosed`.
    StreamStarted {
        window: WindowId,
        codec: Codec,
        color: ColorFormat,
        info: WindowInfo,
    },
    /// Window geometry, offset or title changed; size changes are followed by a keyframe.
    WindowChanged(WindowInfo),
    /// The Mac window closed; client should close its window too.
    WindowClosed { window: WindowId },
    Pong { nonce: u64 },
    Error { window: Option<WindowId>, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u8),
}

/// Input events, client to host, on their own stream so a big control message never delays them.
/// Coordinates are window-relative surface pixels (same space as `WindowInfo::width/height`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove { window: WindowId, x: f32, y: f32 },
    MouseButton { window: WindowId, x: f32, y: f32, button: MouseButton, pressed: bool },
    /// Scroll delta in pixels; `precise` is true for touchpads / smooth scrolling.
    Scroll { window: WindowId, dx: f32, dy: f32, precise: bool },
    /// macOS virtual keycode (`kVK_*`), see [`keys`].
    Key { window: WindowId, keycode: u16, pressed: bool },
    /// Client lost focus: release every key and button the host still holds down.
    ReleaseAll,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
}

/// Encodes a message as `[u32 LE length][postcard bytes]`.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_FRAME_LEN {
        return Err(CodecError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn decode_body<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, CodecError> {
    Ok(postcard::from_bytes(body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let msg = ClientMsg::Subscribe { window: 7, max_fps: 60, bitrate_kbps: 30_000 };
        let frame = encode_frame(&msg).unwrap();
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        assert_eq!(decode_body::<ClientMsg>(&frame[4..]).unwrap(), msg);
    }
}
