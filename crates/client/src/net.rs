//! Connection to the host: handshake, control and input streams, video datagram routing and
//! one decode thread per window. Runs on the tokio runtime; talks to the UI via [`UiEvent`]
//! (through a [`UiSender`]) and [`NetCmd`].

use std::{
    collections::{HashMap, HashSet},
    sync::mpsc::{SyncSender, TrySendError, sync_channel},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use umac_proto::{
    ClientMsg, Codec, ColorFormat, DisplayInfo, Hello, HelloReply, HostMsg, InputEvent, PROTOCOL_VERSION, WindowId,
    WindowInfo, WindowKind,
    video::{HEADER_LEN, ShardHeader},
};
use umac_transport::{
    SERVER_NAME, client_endpoint,
    fec::{self, Output, Reassembler},
    framing::{read_msg, write_msg},
    quinn,
};
use crate::{
    config::{Config, HostEntry, Store},
    core::MAX_POPUPS,
    decode,
    gate::KeyframeGate,
    ui::{UiEvent, UiSender},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(20);
const STATS_EVERY: Duration = Duration::from_secs(5);
/// Encoded frames queued for a decode thread. More means the decoder is behind; drop instead.
const DECODE_QUEUE: usize = 3;

pub enum NetCmd {
    Control(ClientMsg),
    Input(InputEvent),
    DecodeError(WindowId),
}

pub struct Session {
    _endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    ctrl_send: quinn::SendStream,
    ctrl_recv: quinn::RecvStream,
    pub host_name: String,
}

fn client_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "umac-client".to_owned())
}

pub async fn connect(store: &Store, host: &HostEntry, pairing_token: Option<String>) -> Result<Session> {
    let identity = store.identity()?;
    let endpoint = client_endpoint(&identity, host.fingerprint()?)?;
    let addr = tokio::net::lookup_host(&host.addr)
        .await
        .with_context(|| format!("resolving {}", host.addr))?
        .next()
        .with_context(|| format!("{} did not resolve", host.addr))?;
    let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, endpoint.connect(addr, SERVER_NAME)?)
        .await
        .context("connection timed out")?
        .context("connecting (wrong host fingerprint or host not running?)")?;
    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
    let hello = Hello {
        version: PROTOCOL_VERSION,
        client_name: client_name(),
        codecs: vec![Codec::Hevc, Codec::H264],
        pairing_token,
    };
    write_msg(&mut ctrl_send, &hello).await?;
    let reply: Option<HelloReply> =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_msg(&mut ctrl_recv))
            .await
            .context("no reply from host")?
            .context("host dropped the connection during handshake (is this client paired?)")?;
    match reply {
        Some(HelloReply::Accepted { host_name }) => {
            Ok(Session { _endpoint: endpoint, conn, ctrl_send, ctrl_recv, host_name })
        }
        Some(HelloReply::Rejected { reason }) => bail!("host rejected us: {reason}"),
        None => bail!("host closed the connection during handshake"),
    }
}

impl Session {
    pub async fn list_displays(&mut self) -> Result<Vec<DisplayInfo>> {
        write_msg(&mut self.ctrl_send, &ClientMsg::ListDisplays).await?;
        loop {
            match read_msg(&mut self.ctrl_recv).await? {
                Some(HostMsg::Displays(d)) => return Ok(d),
                Some(HostMsg::Error { message, .. }) => bail!("host error: {message}"),
                Some(_) => continue,
                None => bail!("host closed the control stream"),
            }
        }
    }

    pub async fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
        write_msg(&mut self.ctrl_send, &ClientMsg::ListWindows).await?;
        loop {
            match read_msg(&mut self.ctrl_recv).await? {
                Some(HostMsg::Windows(w)) => return Ok(w),
                Some(HostMsg::Error { message, .. }) => bail!("host error: {message}"),
                Some(_) => continue,
                None => bail!("host closed the control stream"),
            }
        }
    }
}

/// Which windows and displays to open.
#[derive(Debug, Default, Clone)]
pub struct Targets {
    pub windows: Vec<WindowId>,
    pub apps: Vec<String>,
    /// Display indexes from `umac displays`, shown as whole desktops.
    pub displays: Vec<u32>,
    /// Desktop bitrate in kbit/s; derived from the display size when unset.
    pub desktop_bitrate_kbps: Option<u32>,
}

/// Highest automatic desktop bitrate: the Mac may be on Wi-Fi, and FEC adds on top.
const MAX_DESKTOP_KBPS: u32 = 80_000;
const MIN_DESKTOP_KBPS: u32 = 5_000;

/// Scales the per-window bitrate (meant for about 1080p) with the display's pixel count.
pub fn desktop_bitrate(base_kbps: u32, width: u32, height: u32) -> u32 {
    let pixels = width as u64 * height as u64;
    let scaled = base_kbps as u64 * pixels / (1920 * 1080);
    scaled.clamp(MIN_DESKTOP_KBPS as u64, MAX_DESKTOP_KBPS as u64) as u32
}

impl Targets {
    pub fn resolve(&self, available: &[WindowInfo]) -> Vec<WindowId> {
        let mut ids: Vec<WindowId> = Vec::new();
        for w in available {
            let by_id = self.windows.contains(&w.id);
            let by_app = w.kind == WindowKind::Normal
                && self.apps.iter().any(|a| {
                    w.app_name.eq_ignore_ascii_case(a) || w.bundle_id.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(a))
                });
            if (by_id || by_app) && !ids.contains(&w.id) {
                ids.push(w.id);
            }
        }
        ids
    }
}

struct Stream {
    /// Set for popups; they close together with their parent.
    parent: Option<WindowId>,
    reassembler: Reassembler,
    gate: KeyframeGate,
    decode_tx: SyncSender<fec::Frame>,
    frames: u32,
    bytes: u64,
    losses: u32,
}

pub async fn run_open(
    mut session: Session,
    targets: Targets,
    cfg: Config,
    ui: UiSender,
    cmd_tx: mpsc::UnboundedSender<NetCmd>,
    mut cmd_rx: mpsc::UnboundedReceiver<NetCmd>,
) -> Result<()> {
    let ids = if targets.windows.is_empty() && targets.apps.is_empty() {
        Vec::new()
    } else {
        let ids = targets.resolve(&session.list_windows().await?);
        if ids.is_empty() {
            bail!("no matching windows; see `umac list`");
        }
        ids
    };
    let displays = if targets.displays.is_empty() { Vec::new() } else { session.list_displays().await? };
    let conn = session.conn.clone();
    let mut input = conn.open_uni().await?;
    let mut pending: HashSet<WindowId> = HashSet::new();
    for &window in &ids {
        let msg = ClientMsg::Subscribe { window, max_fps: cfg.max_fps, bitrate_kbps: cfg.bitrate_kbps };
        write_msg(&mut session.ctrl_send, &msg).await?;
        pending.insert(window);
    }
    for &index in &targets.displays {
        let d = displays.iter().find(|d| d.index == index).with_context(|| format!("no display {index}; see `umac displays`"))?;
        let bitrate_kbps = targets.desktop_bitrate_kbps.unwrap_or_else(|| desktop_bitrate(cfg.bitrate_kbps, d.width, d.height));
        tracing::info!(display = index, w = d.width, h = d.height, bitrate_kbps, "opening desktop");
        write_msg(&mut session.ctrl_send, &ClientMsg::SubscribeDisplay { index, max_fps: cfg.max_fps, bitrate_kbps }).await?;
        pending.insert(umac_proto::desktop_id(index));
    }

    // read_msg is not cancel safe, so control messages get their own reader task.
    let (host_tx, mut host_rx) = mpsc::channel::<HostMsg>(64);
    let mut ctrl_recv = session.ctrl_recv;
    tokio::spawn(async move {
        loop {
            match read_msg::<_, HostMsg>(&mut ctrl_recv).await {
                Ok(Some(m)) => {
                    if host_tx.send(m).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("control stream: {e}");
                    break;
                }
            }
        }
    });

    let mut streams: HashMap<WindowId, Stream> = HashMap::new();
    let mut ctrl_send = session.ctrl_send;
    let mut tick = tokio::time::interval(TICK);
    let mut last_stats = Instant::now();

    loop {
        tokio::select! {
            dgram = conn.read_datagram() => {
                let dgram = match dgram {
                    Ok(d) => d,
                    Err(e) => bail!("connection lost: {e}"),
                };
                let Ok(h) = ShardHeader::parse(&dgram) else { continue };
                let Some(s) = streams.get_mut(&h.window) else { continue };
                s.bytes += dgram.len() as u64;
                let now = Instant::now();
                for out in s.reassembler.push(h, &dgram[HEADER_LEN..], now) {
                    match out {
                        Output::Lost => {
                            s.losses += 1;
                            if s.gate.on_loss(now) {
                                write_msg(&mut ctrl_send, &ClientMsg::RequestKeyframe { window: h.window }).await?;
                            }
                        }
                        Output::Frame(f) => {
                            if !s.gate.accept(f.meta.keyframe) {
                                continue;
                            }
                            match s.decode_tx.try_send(f) {
                                Ok(()) => s.frames += 1,
                                Err(TrySendError::Full(_)) => {
                                    if s.gate.on_loss(now) {
                                        write_msg(&mut ctrl_send, &ClientMsg::RequestKeyframe { window: h.window }).await?;
                                    }
                                }
                                Err(TrySendError::Disconnected(_)) => {}
                            }
                        }
                    }
                }
            }
            msg = host_rx.recv() => {
                let Some(msg) = msg else { bail!("host closed the control stream") };
                match msg {
                    HostMsg::StreamStarted { window, codec, color, info } => {
                        let requested = pending.remove(&window);
                        if !requested && !accept_popup(&info, &streams) {
                            tracing::debug!(window, parent = ?info.parent, "refusing unsolicited stream");
                            write_msg(&mut ctrl_send, &ClientMsg::Unsubscribe { window }).await?;
                            continue;
                        }
                        if streams.contains_key(&window) {
                            continue;
                        }
                        tracing::info!(window, ?codec, kind = ?info.kind, w = info.width, h = info.height, "stream started: {}", info.title);
                        let decode_tx = spawn_decoder(window, codec, color, ui.clone(), cmd_tx.clone())?;
                        let parent = if requested { None } else { info.parent };
                        streams.insert(window, Stream {
                            parent,
                            reassembler: Reassembler::default(),
                            gate: KeyframeGate::new(Instant::now()),
                            decode_tx,
                            frames: 0,
                            bytes: 0,
                            losses: 0,
                        });
                        ui.send(UiEvent::Open(info));
                    }
                    HostMsg::WindowChanged(info) if streams.contains_key(&info.id) => {
                        ui.send(UiEvent::Changed(info));
                    }
                    HostMsg::WindowClosed { window } => {
                        pending.remove(&window);
                        for w in remove_with_popups(&mut streams, window) {
                            ui.send(UiEvent::Closed(w));
                        }
                    }
                    HostMsg::Error { window, message } => {
                        tracing::warn!(?window, "host error: {message}");
                        if let Some(w) = window
                            && pending.remove(&w)
                        {
                            ui.send(UiEvent::Closed(w));
                        }
                    }
                    HostMsg::Clipboard { text } => {
                        if cfg.clipboard_to_pc && text.len() <= umac_proto::MAX_CLIPBOARD_BYTES {
                            tracing::debug!(bytes = text.len(), "Mac clipboard received");
                            ui.send(UiEvent::Clipboard(text));
                        }
                    }
                    HostMsg::ClipboardRequest { nonce } => {
                        ui.send(UiEvent::ClipboardRequest(nonce));
                    }
                    HostMsg::Cursor(c) => {
                        ui.send(UiEvent::Cursor { id: c.id, shape: Some(c.shape) });
                    }
                    HostMsg::CursorKnown { id } => {
                        ui.send(UiEvent::Cursor { id, shape: None });
                    }
                    other => tracing::debug!(?other, "ignored host message"),
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    NetCmd::Control(msg) => {
                        if let ClientMsg::Unsubscribe { window } = msg {
                            pending.remove(&window);
                            write_msg(&mut ctrl_send, &msg).await?;
                            for w in remove_with_popups(&mut streams, window).into_iter().filter(|&w| w != window) {
                                write_msg(&mut ctrl_send, &ClientMsg::Unsubscribe { window: w }).await?;
                            }
                            continue;
                        }
                        write_msg(&mut ctrl_send, &msg).await?;
                    }
                    NetCmd::Input(ev) => write_msg(&mut input, &ev).await?,
                    NetCmd::DecodeError(window) => {
                        if let Some(s) = streams.get_mut(&window)
                            && s.gate.on_loss(Instant::now())
                        {
                            write_msg(&mut ctrl_send, &ClientMsg::RequestKeyframe { window }).await?;
                        }
                    }
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                for (&window, s) in streams.iter_mut() {
                    let mut request = false;
                    if s.reassembler.expire(now) {
                        s.losses += 1;
                        request |= s.gate.on_loss(now);
                    }
                    request |= s.gate.poll(now);
                    if request {
                        write_msg(&mut ctrl_send, &ClientMsg::RequestKeyframe { window }).await?;
                    }
                }
                if now.duration_since(last_stats) >= STATS_EVERY {
                    let secs = now.duration_since(last_stats).as_secs_f64();
                    for (&window, s) in streams.iter_mut() {
                        tracing::info!(
                            window,
                            fps = format_args!("{:.1}", s.frames as f64 / secs),
                            mbps = format_args!("{:.1}", s.bytes as f64 * 8.0 / secs / 1e6),
                            losses = s.losses,
                            rtt_ms = format_args!("{:.1}", conn.rtt().as_secs_f64() * 1e3),
                            "net"
                        );
                        (s.frames, s.bytes, s.losses) = (0, 0, 0);
                    }
                    last_stats = now;
                }
            }
        }
        if pending.is_empty() && streams.values().all(|s| s.parent.is_some()) {
            break;
        }
    }
    conn.close(0u32.into(), b"bye");
    Ok(())
}

/// Popups are accepted only as children of a window being streamed, and only up to a cap.
fn accept_popup(info: &WindowInfo, streams: &HashMap<WindowId, Stream>) -> bool {
    info.kind == WindowKind::Popup
        && info.parent.is_some_and(|p| streams.contains_key(&p))
        && streams.values().filter(|s| s.parent.is_some()).count() < MAX_POPUPS
}

/// Removes `window` and every popup below it; returns the ids that were streaming.
fn remove_with_popups(streams: &mut HashMap<WindowId, Stream>, window: WindowId) -> Vec<WindowId> {
    let mut removed = Vec::new();
    let mut frontier = vec![window];
    while let Some(w) = frontier.pop() {
        if streams.remove(&w).is_some() {
            removed.push(w);
        }
        frontier.extend(streams.iter().filter(|(_, s)| s.parent == Some(w)).map(|(&id, _)| id));
    }
    removed
}

fn spawn_decoder(
    window: WindowId,
    codec: Codec,
    color: ColorFormat,
    ui: UiSender,
    cmd_tx: mpsc::UnboundedSender<NetCmd>,
) -> Result<SyncSender<fec::Frame>> {
    let mut decoder = decode::create(codec, color)?;
    tracing::info!(window, "decoder: {}", decoder.describe());
    let (tx, rx) = sync_channel::<fec::Frame>(DECODE_QUEUE);
    std::thread::Builder::new().name(format!("decode-{window}")).spawn(move || {
        let (mut n, mut total, mut since) = (0u32, Duration::ZERO, Instant::now());
        while let Ok(f) = rx.recv() {
            let t = Instant::now();
            match decoder.decode(&f.data) {
                Ok(Some(frame)) => {
                    total += t.elapsed();
                    n += 1;
                    if !ui.send(UiEvent::Frame { window, frame }) {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(window, "decode failed: {e}");
                    let _ = cmd_tx.send(NetCmd::DecodeError(window));
                }
            }
            if since.elapsed() >= STATS_EVERY && n > 0 {
                tracing::info!(window, decode_ms = format_args!("{:.2}", total.as_secs_f64() * 1e3 / n as f64), "decode");
                (n, total, since) = (0, Duration::ZERO, Instant::now());
            }
        }
    })?;
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_bitrate_scales_with_pixels() {
        assert_eq!(desktop_bitrate(25_000, 1920, 1080), 25_000);
        assert_eq!(desktop_bitrate(25_000, 2560, 1440), 44_444);
        assert_eq!(desktop_bitrate(25_000, 3840, 2160), MAX_DESKTOP_KBPS);
        assert_eq!(desktop_bitrate(25_000, 640, 360), MIN_DESKTOP_KBPS);
    }

    fn win(id: WindowId, app: &str, kind: WindowKind) -> WindowInfo {
        WindowInfo {
            id,
            pid: 1,
            app_name: app.into(),
            bundle_id: Some(format!("com.example.{app}")),
            title: String::new(),
            width: 10,
            height: 10,
            scale: 1.0,
            parent: None,
            offset: (0, 0),
            kind,
        }
    }

    fn stream(parent: Option<WindowId>) -> Stream {
        Stream {
            parent,
            reassembler: Reassembler::default(),
            gate: KeyframeGate::new(Instant::now()),
            decode_tx: sync_channel(1).0,
            frames: 0,
            bytes: 0,
            losses: 0,
        }
    }

    #[test]
    fn popups_need_a_streamed_parent_and_close_with_it() {
        let mut streams = HashMap::from([(1, stream(None)), (2, stream(Some(1))), (3, stream(Some(2))), (9, stream(None))]);
        let mut popup = win(4, "Safari", WindowKind::Popup);
        popup.parent = Some(1);
        assert!(accept_popup(&popup, &streams));
        popup.parent = Some(77);
        assert!(!accept_popup(&popup, &streams));
        popup.parent = None;
        assert!(!accept_popup(&popup, &streams));
        assert!(!accept_popup(&win(5, "Safari", WindowKind::Normal), &streams));

        let mut removed = remove_with_popups(&mut streams, 1);
        removed.sort();
        assert_eq!(removed, vec![1, 2, 3]);
        assert_eq!(streams.keys().copied().collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn popup_cap() {
        let mut streams: HashMap<WindowId, Stream> = HashMap::from([(1, stream(None))]);
        for i in 0..MAX_POPUPS as u32 {
            streams.insert(100 + i, stream(Some(1)));
        }
        let mut popup = win(4, "Safari", WindowKind::Popup);
        popup.parent = Some(1);
        assert!(!accept_popup(&popup, &streams));
    }

    #[test]
    fn resolves_targets() {
        let all = [win(1, "Safari", WindowKind::Normal), win(2, "Safari", WindowKind::Popup), win(3, "Notes", WindowKind::Normal)];
        let t = Targets { windows: vec![3], apps: vec!["safari".into()], ..Default::default() };
        assert_eq!(t.resolve(&all), vec![1, 3]);
        let t = Targets { windows: vec![], apps: vec!["com.example.Notes".into()], ..Default::default() };
        assert_eq!(t.resolve(&all), vec![3]);
        assert!(Targets::default().resolve(&all).is_empty());
    }
}
