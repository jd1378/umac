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

/// Largest clipboard text either side sends; bigger copies are skipped, not truncated.
pub const MAX_CLIPBOARD_BYTES: usize = 900 * 1024;

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
    /// A whole display, streamed with the Mac cursor drawn in.
    Desktop,
}

/// Stream ids at or above this are displays, not windows. `CGWindowID`s stay far below it.
pub const DESKTOP_ID_BASE: WindowId = 0x8000_0000;

/// Stream id for display `index` (position in `HostMsg::Displays`).
pub fn desktop_id(index: u32) -> WindowId {
    DESKTOP_ID_BASE | (index & !DESKTOP_ID_BASE)
}

/// Display index of a desktop stream id, or `None` for a window id.
pub fn desktop_index(id: WindowId) -> Option<u32> {
    (id & DESKTOP_ID_BASE != 0).then_some(id & !DESKTOP_ID_BASE)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Index used by `SubscribeDisplay`; stable while the display set does not change.
    pub index: u32,
    pub name: String,
    /// Size in pixels (backing store, so 2x points on HiDPI modes).
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub main: bool,
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
    ListDisplays,
    /// Stream a whole display. The host replies with `StreamStarted` (kind `Desktop`, id from
    /// [`desktop_id`]) or `Error`; all other messages then use that id.
    SubscribeDisplay { index: u32, max_fps: u16, bitrate_kbps: u32 },
    /// Runs one of a fixed set of Mac actions and reports the window it opened, which the
    /// client then subscribes to. Never arbitrary commands.
    Launch { nonce: u64, action: LaunchAction },
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
    /// The Mac clipboard changed (text only).
    Clipboard { text: String },
    /// A paste is about to happen on the Mac (e.g. a context-menu Paste item was clicked);
    /// the client answers with `InputEvent::SetClipboard` carrying the same nonce.
    ClipboardRequest { nonce: u64 },
    Displays(Vec<DisplayInfo>),
    /// The Mac cursor over a streamed window changed shape.
    Cursor(Cursor),
    /// Switch back to a cursor image sent earlier with this id (the client caches a few).
    CursorKnown { id: u64 },
    /// Answer to `Launch`: the new (or reused) window to stream, or why it failed.
    Launched { nonce: u64, result: Result<WindowInfo, String> },
}

/// Largest cursor image accepted, per side, in pixels.
pub const MAX_CURSOR_SIDE: u32 = 256;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    /// Content hash; the same image always has the same id within one host run.
    pub id: u64,
    pub shape: CursorShape,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CursorShape {
    Named(NamedCursor),
    /// Unpremultiplied RGBA, row-major, `width * height * 4` bytes. Hotspot in pixels.
    Image { width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8> },
}

/// Standard cursor shapes, named after the CSS / freedesktop cursor names so clients can use
/// their native themed cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamedCursor {
    Default,
    Text,
    VerticalText,
    Pointer,
    Grab,
    Grabbing,
    Crosshair,
    NotAllowed,
    Copy,
    Alias,
    ContextMenu,
    ColResize,
    RowResize,
    EwResize,
    NsResize,
    EResize,
    WResize,
    NResize,
    SResize,
    NeResize,
    NwResize,
    SeResize,
    SwResize,
    NeswResize,
    NwseResize,
    ZoomIn,
    ZoomOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchAction {
    /// The macOS Apps launcher.
    Apps,
    /// A new Terminal window in the home directory.
    Terminal,
    ActivityMonitor,
    /// A Finder window on /Applications (fallback launcher).
    FinderApplications,
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
    /// Scroll delta. `precise` (touchpads, smooth scrolling): pixels. Otherwise (mouse wheels):
    /// lines, possibly fractional for high-resolution wheels.
    Scroll { window: WindowId, dx: f32, dy: f32, precise: bool },
    /// macOS virtual keycode (`kVK_*`), see [`keys`].
    Key { window: WindowId, keycode: u16, pressed: bool },
    /// Client lost focus: release every key and button the host still holds down.
    ReleaseAll,
    /// Puts text on the Mac clipboard before the input events that follow it (a paste).
    /// `nonce` answers a `HostMsg::ClipboardRequest`; `text: None` means the client declined.
    SetClipboard { nonce: Option<u64>, text: Option<String> },
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
    fn desktop_ids_do_not_collide_with_windows() {
        assert_eq!(desktop_index(desktop_id(0)), Some(0));
        assert_eq!(desktop_index(desktop_id(3)), Some(3));
        assert_eq!(desktop_index(612), None);
        assert_eq!(desktop_index(DESKTOP_ID_BASE - 1), None);
        // An out-of-range index never escapes into window id space.
        assert!(desktop_index(desktop_id(u32::MAX)).is_some());
    }

    #[test]
    fn frame_roundtrip() {
        let msg = ClientMsg::Subscribe { window: 7, max_fps: 60, bitrate_kbps: 30_000 };
        let frame = encode_frame(&msg).unwrap();
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        assert_eq!(decode_body::<ClientMsg>(&frame[4..]).unwrap(), msg);
    }
}
