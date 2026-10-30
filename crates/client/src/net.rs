//! Connection to the host: handshake, control and input streams, video datagram routing and
//! one decode thread per window. Runs on the tokio runtime; talks to the UI via [`UiEvent`]
//! (through the winit proxy) and [`NetCmd`].

use std::{
    collections::{HashMap, HashSet},
    sync::mpsc::{SyncSender, TrySendError, sync_channel},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use umac_proto::{
    ClientMsg, Codec, ColorFormat, Hello, HelloReply, HostMsg, InputEvent, PROTOCOL_VERSION, WindowId,
    WindowInfo, WindowKind,
    video::{HEADER_LEN, ShardHeader},
};
use umac_transport::{
    SERVER_NAME, client_endpoint,
    fec::{self, Output, Reassembler},
    framing::{read_msg, write_msg},
    quinn,
};
use winit::event_loop::EventLoopProxy;

use crate::{
    config::{Config, HostEntry, Store},
    decode::{self, Nv12Frame},
    gate::KeyframeGate,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(20);
const STATS_EVERY: Duration = Duration::from_secs(5);
/// Encoded frames queued for a decode thread. More means the decoder is behind; drop instead.
const DECODE_QUEUE: usize = 3;

pub enum UiEvent {
    Open(WindowInfo),
    Frame { window: WindowId, frame: Nv12Frame },
    Changed(WindowInfo),
    Closed(WindowId),
    Shutdown(String),
}

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

/// Which windows to open.
#[derive(Debug, Default, Clone)]
pub struct Targets {
    pub windows: Vec<WindowId>,
    pub apps: Vec<String>,
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
    proxy: EventLoopProxy<UiEvent>,
    cmd_tx: mpsc::UnboundedSender<NetCmd>,
    mut cmd_rx: mpsc::UnboundedReceiver<NetCmd>,
) -> Result<()> {
    let available = session.list_windows().await?;
    let ids = targets.resolve(&available);
    if ids.is_empty() {
        bail!("no matching windows; see `umac list`");
    }
    let conn = session.conn.clone();
    let mut input = conn.open_uni().await?;
    let mut pending: HashSet<WindowId> = HashSet::new();
    for &window in &ids {
        let msg = ClientMsg::Subscribe { window, max_fps: cfg.max_fps, bitrate_kbps: cfg.bitrate_kbps };
        write_msg(&mut session.ctrl_send, &msg).await?;
        pending.insert(window);
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
                    HostMsg::StreamStarted { window, codec, color, info } if pending.remove(&window) => {
                        tracing::info!(window, ?codec, w = info.width, h = info.height, "stream started: {}", info.title);
                        let decode_tx = spawn_decoder(window, codec, color, proxy.clone(), cmd_tx.clone())?;
                        streams.insert(window, Stream {
                            reassembler: Reassembler::default(),
                            gate: KeyframeGate::new(Instant::now()),
                            decode_tx,
                            frames: 0,
                            bytes: 0,
                            losses: 0,
                        });
                        let _ = proxy.send_event(UiEvent::Open(info));
                    }
                    HostMsg::WindowChanged(info) if streams.contains_key(&info.id) => {
                        let _ = proxy.send_event(UiEvent::Changed(info));
                    }
                    HostMsg::WindowClosed { window } => {
                        pending.remove(&window);
                        if streams.remove(&window).is_some() {
                            let _ = proxy.send_event(UiEvent::Closed(window));
                        }
                    }
                    HostMsg::Error { window, message } => {
                        tracing::warn!(?window, "host error: {message}");
                        if let Some(w) = window
                            && pending.remove(&w)
                        {
                            let _ = proxy.send_event(UiEvent::Closed(w));
                        }
                    }
                    other => tracing::debug!(?other, "ignored host message"),
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    NetCmd::Control(msg) => {
                        if let ClientMsg::Unsubscribe { window } = msg {
                            streams.remove(&window);
                            pending.remove(&window);
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
        if streams.is_empty() && pending.is_empty() {
            break;
        }
    }
    conn.close(0u32.into(), b"bye");
    Ok(())
}

fn spawn_decoder(
    window: WindowId,
    codec: Codec,
    color: ColorFormat,
    proxy: EventLoopProxy<UiEvent>,
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
                    if proxy.send_event(UiEvent::Frame { window, frame }).is_err() {
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

    #[test]
    fn resolves_targets() {
        let all = [win(1, "Safari", WindowKind::Normal), win(2, "Safari", WindowKind::Popup), win(3, "Notes", WindowKind::Normal)];
        let t = Targets { windows: vec![3], apps: vec!["safari".into()] };
        assert_eq!(t.resolve(&all), vec![1, 3]);
        let t = Targets { windows: vec![], apps: vec!["com.example.Notes".into()] };
        assert_eq!(t.resolve(&all), vec![3]);
        assert!(Targets::default().resolve(&all).is_empty());
    }
}
