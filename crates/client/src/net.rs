//! Connection to the host: handshake, control and input streams, video datagram routing and
//! one decode thread per window. Runs on the tokio runtime; talks to the UI via [`UiEvent`]
//! (through a [`UiSender`]) and [`NetCmd`].

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tokio::sync::{mpsc, oneshot};
use umac_proto::{
    ClientMsg, Codec, ColorFormat, DisplayInfo, Hello, HelloReply, HostMsg, InputEvent, LaunchAction, PROTOCOL_VERSION, WindowId,
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
    core::{self, MAX_POPUPS},
    decode,
    gate::KeyframeGate,
    ids::{HostKey, IdMap},
    latency::{self, ClockSync, FrameTiming, HostStages},
    ui::{FrameSlot, UiEvent, UiSender},
};

/// Clock probes: a burst when the session starts, then a few every interval.
const PROBE_EVERY: Duration = Duration::from_secs(2);
const PROBES_PER_ROUND: u64 = 4;
/// Frames whose first datagram time is remembered while they arrive.
const MAX_IN_FLIGHT: usize = 64;

/// An encoded frame on its way to the decoder, with when it arrived.
struct Arrived {
    frame: fec::Frame,
    capture: Option<Instant>,
    first_datagram: Instant,
    complete: Instant,
}

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


/// Highest automatic desktop bitrate: the Mac may be on Wi-Fi, and FEC adds on top.
const MAX_DESKTOP_KBPS: u32 = 80_000;
const MIN_DESKTOP_KBPS: u32 = 5_000;

/// Scales the per-window bitrate (meant for about 1080p) with the display's pixel count.
pub fn desktop_bitrate(base_kbps: u32, width: u32, height: u32) -> u32 {
    let pixels = width as u64 * height as u64;
    let scaled = base_kbps as u64 * pixels / (1920 * 1080);
    scaled.clamp(MIN_DESKTOP_KBPS as u64, MAX_DESKTOP_KBPS as u64) as u32
}

/// Windows matching explicit ids or apps (by name or bundle id; normal windows only).
pub fn resolve(windows: &[WindowId], apps: &[String], available: &[WindowInfo]) -> Vec<WindowId> {
    let mut ids: Vec<WindowId> = Vec::new();
    for w in available {
        let by_id = windows.contains(&w.id);
        let by_app = w.kind == WindowKind::Normal
            && apps.iter().any(|a| w.app_name.eq_ignore_ascii_case(a) || w.bundle_id.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(a)));
        if (by_id || by_app) && !ids.contains(&w.id) {
            ids.push(w.id);
        }
    }
    ids
}

pub type Reply<T> = oneshot::Sender<Result<T>>;

/// Requests the manager hands to a host's session task.
pub enum HostReq {
    /// Streams windows by id or app; already shown ones are raised instead.
    Open { windows: Vec<WindowId>, apps: Vec<String>, reply: Reply<String> },
    Launch { action: LaunchAction, reply: Reply<String> },
    /// A whole display (the main one when `display` is `None`).
    Desktop { display: Option<u32>, bitrate_kbps: Option<u32>, reply: Reply<String> },
    List { reply: Reply<Vec<WindowInfo>> },
    Displays { reply: Reply<Vec<DisplayInfo>> },
    /// Ends the session cleanly (daemon exit).
    Close,
}

/// A request waiting for a `Windows` or `Displays` answer. The host answers list requests in
/// order, so they queue up.
enum Query {
    List(Reply<Vec<WindowInfo>>),
    Open { windows: Vec<WindowId>, apps: Vec<String>, reply: Reply<String> },
    Displays(Reply<Vec<DisplayInfo>>),
    Desktop { display: Option<u32>, bitrate_kbps: Option<u32>, reply: Reply<String> },
}

impl Query {
    fn fail(self, e: String) {
        match self {
            Query::List(r) => drop(r.send(Err(anyhow::anyhow!(e)))),
            Query::Open { reply, .. } | Query::Desktop { reply, .. } => drop(reply.send(Err(anyhow::anyhow!(e)))),
            Query::Displays(r) => drop(r.send(Err(anyhow::anyhow!(e)))),
        }
    }
}

/// What a host's session task needs besides its connection.
pub struct HostCtx {
    pub key: HostKey,
    pub ids: Arc<Mutex<IdMap>>,
    pub ui: UiSender,
    pub cfg: Config,
}

struct Stream {
    /// Set for popups and dialogs; they close together with their parent.
    parent: Option<WindowId>,
    pid: i32,
    kind: WindowKind,
    /// Opened by the host on its own (popup, new window, dialog), not requested here.
    auto: bool,
    /// Id the UI knows this window by.
    local: WindowId,
    reassembler: Reassembler,
    gate: KeyframeGate,
    decode_tx: SyncSender<Arrived>,
    /// When the first datagram of each frame still being assembled arrived.
    first_seen: HashMap<u32, Instant>,
    frames: u32,
    bytes: u64,
    losses: u32,
}

/// One host's session: streams any number of windows until the connection drops or the
/// manager closes it. Every window it showed is closed in the UI when it ends.
pub async fn run_host(
    session: Session,
    ctx: HostCtx,
    mut req_rx: mpsc::UnboundedReceiver<HostReq>,
    cmd_tx: mpsc::UnboundedSender<NetCmd>,
    mut cmd_rx: mpsc::UnboundedReceiver<NetCmd>,
) -> Result<()> {
    let conn = session.conn.clone();
    let mut streams: HashMap<WindowId, Stream> = HashMap::new();
    let result = host_loop(session, &ctx, &mut req_rx, &cmd_tx, &mut cmd_rx, &mut streams).await;
    conn.close(0u32.into(), b"bye");
    let gone = ctx.ids.lock().unwrap().release_host(&ctx.key);
    for local in gone {
        ctx.ui.send(UiEvent::Closed(local));
    }
    result
}

async fn host_loop(
    session: Session,
    ctx: &HostCtx,
    req_rx: &mut mpsc::UnboundedReceiver<HostReq>,
    cmd_tx: &mpsc::UnboundedSender<NetCmd>,
    cmd_rx: &mut mpsc::UnboundedReceiver<NetCmd>,
    streams: &mut HashMap<WindowId, Stream>,
) -> Result<()> {
    let (cfg, ui) = (&ctx.cfg, &ctx.ui);
    let conn = session.conn.clone();
    let mut input = conn.open_uni().await?;
    let mut pending: HashSet<WindowId> = HashSet::new();
    let mut queries: VecDeque<Query> = VecDeque::new();
    let mut launches: HashMap<u64, (LaunchAction, Reply<String>)> = HashMap::new();
    let mut nonce = 0u64;

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
                // Also how it ends when we close the session ourselves; the session loop
                // reports real failures.
                Err(e) => {
                    tracing::debug!("control stream: {e}");
                    break;
                }
            }
        }
    });

    let mut ctrl_send = session.ctrl_send;
    let mut tick = tokio::time::interval(TICK);
    let mut last_stats = Instant::now();
    let mut clock = ClockSync::default();
    let mut probe = tokio::time::interval(PROBE_EVERY);
    let mut probe_nonce = 0u64;
    if cfg.show_stats {
        write_msg(&mut ctrl_send, &ClientMsg::WantStats { on: true }).await?;
    }

    // Streams a Mac window, or brings it to the front if it is already shown.
    macro_rules! show {
        ($mac:expr) => {{
            let mac: WindowId = $mac;
            match plan_show(streams, &pending, mac) {
                Show::Raise(local) => {
                    ui.send(UiEvent::Raise(local));
                }
                Show::Subscribe => {
                    pending.insert(mac);
                    write_msg(&mut ctrl_send, &ClientMsg::Subscribe { window: mac, max_fps: cfg.max_fps, bitrate_kbps: cfg.bitrate_kbps }).await?;
                }
                Show::Wait => {}
            }
        }};
    }

    loop {
        tokio::select! {
            req = req_rx.recv() => {
                let Some(req) = req else { return Ok(()) };
                match req {
                    HostReq::Close => return Ok(()),
                    HostReq::Open { windows, apps, reply } => {
                        write_msg(&mut ctrl_send, &ClientMsg::ListWindows).await?;
                        queries.push_back(Query::Open { windows, apps, reply });
                    }
                    HostReq::List { reply } => {
                        write_msg(&mut ctrl_send, &ClientMsg::ListWindows).await?;
                        queries.push_back(Query::List(reply));
                    }
                    HostReq::Displays { reply } => {
                        write_msg(&mut ctrl_send, &ClientMsg::ListDisplays).await?;
                        queries.push_back(Query::Displays(reply));
                    }
                    HostReq::Desktop { display, bitrate_kbps, reply } => {
                        write_msg(&mut ctrl_send, &ClientMsg::ListDisplays).await?;
                        queries.push_back(Query::Desktop { display, bitrate_kbps, reply });
                    }
                    HostReq::Launch { action, reply } => {
                        nonce += 1;
                        write_msg(&mut ctrl_send, &ClientMsg::Launch { nonce, action }).await?;
                        launches.insert(nonce, (action, reply));
                    }
                }
            }
            _ = probe.tick() => {
                for _ in 0..PROBES_PER_ROUND {
                    probe_nonce += 1;
                    write_msg(&mut ctrl_send, &ClientMsg::TimeProbe { nonce: probe_nonce, client_us: latency::now_us() }).await?;
                }
            }
            dgram = conn.read_datagram() => {
                let dgram = match dgram {
                    Ok(d) => d,
                    Err(e) => bail!("connection lost: {e}"),
                };
                let Ok(h) = ShardHeader::parse(&dgram) else { continue };
                let Some(s) = streams.get_mut(&h.window) else { continue };
                s.bytes += dgram.len() as u64;
                let now = Instant::now();
                if s.first_seen.len() >= MAX_IN_FLIGHT {
                    s.first_seen.clear();
                }
                s.first_seen.entry(h.frame_seq).or_insert(now);
                for out in s.reassembler.push(h, &dgram[HEADER_LEN..], now) {
                    match out {
                        Output::Lost => {
                            s.losses += 1;
                            if s.gate.on_loss(now) {
                                write_msg(&mut ctrl_send, &ClientMsg::RequestKeyframe { window: h.window }).await?;
                            }
                        }
                        Output::Frame(f) => {
                            let first_datagram = s.first_seen.remove(&f.meta.seq).unwrap_or(now);
                            s.first_seen.retain(|&seq, _| seq.wrapping_sub(f.meta.seq) < u32::MAX / 2);
                            if !s.gate.accept(f.meta.keyframe) {
                                continue;
                            }
                            let capture = clock.capture_instant(f.meta.capture_us, now);
                            match s.decode_tx.try_send(Arrived { frame: f, capture, first_datagram, complete: now }) {
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
                    HostMsg::Windows(list) => match queries.pop_front() {
                        Some(Query::List(reply)) => drop(reply.send(Ok(list))),
                        Some(Query::Open { windows, apps, reply }) => {
                            let ids = resolve(&windows, &apps, &list);
                            if ids.is_empty() {
                                let _ = reply.send(Err(anyhow::anyhow!("no matching windows; see `umac list`")));
                                continue;
                            }
                            for &mac in &ids {
                                show!(mac);
                            }
                            let _ = reply.send(Ok(format!("{} window(s)", ids.len())));
                        }
                        other => {
                            tracing::debug!("window list nobody asked for");
                            if let Some(q) = other {
                                q.fail("unexpected answer from the host".into());
                            }
                        }
                    },
                    HostMsg::Displays(list) => match queries.pop_front() {
                        Some(Query::Displays(reply)) => drop(reply.send(Ok(list))),
                        Some(Query::Desktop { display, bitrate_kbps, reply }) => {
                            let d = match display {
                                Some(i) => list.iter().find(|d| d.index == i),
                                None => list.iter().find(|d| d.main).or(list.first()),
                            };
                            let Some(d) = d else {
                                let _ = reply.send(Err(anyhow::anyhow!("no such display; see `umac displays`")));
                                continue;
                            };
                            let mac = umac_proto::desktop_id(d.index);
                            if let Show::Raise(local) = plan_show(streams, &pending, mac) {
                                ui.send(UiEvent::Raise(local));
                            } else if pending.insert(mac) {
                                let bitrate_kbps = bitrate_kbps.unwrap_or_else(|| desktop_bitrate(cfg.bitrate_kbps, d.width, d.height));
                                tracing::info!(display = d.index, w = d.width, h = d.height, bitrate_kbps, "opening desktop");
                                write_msg(&mut ctrl_send, &ClientMsg::SubscribeDisplay { index: d.index, max_fps: cfg.max_fps, bitrate_kbps }).await?;
                            }
                            let _ = reply.send(Ok(d.name.clone()));
                        }
                        other => {
                            if let Some(q) = other {
                                q.fail("unexpected answer from the host".into());
                            }
                        }
                    },
                    HostMsg::Launched { nonce: answered, result } => {
                        let Some((action, reply)) = launches.remove(&answered) else { continue };
                        match result {
                            Ok(info) => {
                                show!(info.id);
                                let _ = reply.send(Ok(core::title(&info)));
                            }
                            // The Apps launcher is an overlay on recent macOS; show /Applications.
                            Err(e) if action == LaunchAction::Apps => {
                                tracing::info!("{e}; showing /Applications instead");
                                nonce_retry(&mut ctrl_send, &mut nonce, &mut launches, reply).await?;
                            }
                            Err(e) => drop(reply.send(Err(anyhow::anyhow!("host could not launch {action:?}: {e}")))),
                        }
                    }
                    HostMsg::StreamStarted { window, codec, color, info } => {
                        let requested = pending.remove(&window);
                        if streams.contains_key(&window) {
                            continue;
                        }
                        if !requested && !accept_unsolicited(&info, streams, Follow { new_windows: cfg.follow_new_windows, focus: cfg.follow_focus }) {
                            tracing::debug!(window, kind = ?info.kind, parent = ?info.parent, "refusing unsolicited stream");
                            write_msg(&mut ctrl_send, &ClientMsg::Unsubscribe { window }).await?;
                            continue;
                        }
                        tracing::info!(window, ?codec, kind = ?info.kind, w = info.width, h = info.height, "stream started: {}", info.title);
                        let parent = if requested { None } else { info.parent };
                        let (local, local_info) = {
                            let mut ids = ctx.ids.lock().unwrap();
                            let local = ids.local(&ctx.key, window, info.kind, &core::title(&info));
                            let mut li = info.clone();
                            li.id = local;
                            li.parent = parent.and_then(|p| ids.find(&ctx.key, p));
                            (local, li)
                        };
                        let decode_tx = spawn_decoder(window, local, codec, color, ui.clone(), cmd_tx.clone())?;
                        streams.insert(window, Stream {
                            parent,
                            pid: info.pid,
                            kind: info.kind,
                            auto: !requested,
                            local,
                            reassembler: Reassembler::default(),
                            gate: KeyframeGate::new(Instant::now()),
                            decode_tx,
                            first_seen: HashMap::new(),
                            frames: 0,
                            bytes: 0,
                            losses: 0,
                        });
                        ui.send(UiEvent::Open(local_info));
                    }
                    HostMsg::TimeReply { client_us, host_us, .. } => clock.add(client_us, host_us, latency::now_us()),
                    HostMsg::StreamStats { window, encode_p50_us, encode_p95_us, send_p50_us, .. } => {
                        let Some(s) = streams.get(&window) else { continue };
                        let host = HostStages {
                            encode_p50: encode_p50_us,
                            encode_p95: encode_p95_us,
                            send_p50: send_p50_us,
                            uncertainty: clock.uncertainty_us().unwrap_or(0),
                        };
                        ui.send(UiEvent::HostStats { window: s.local, host });
                    }
                    HostMsg::WindowChanged(mut info) => {
                        let Some(s) = streams.get(&info.id) else { continue };
                        let ids = &mut ctx.ids.lock().unwrap();
                        ids.set_title(s.local, &core::title(&info));
                        info.parent = s.parent.and_then(|p| ids.find(&ctx.key, p));
                        info.id = s.local;
                        ui.send(UiEvent::Changed(info));
                    }
                    HostMsg::WindowClosed { window } => {
                        pending.remove(&window);
                        close_streams(ctx, streams, window, true);
                    }
                    HostMsg::Error { window, message } => {
                        tracing::warn!(?window, "host error: {message}");
                        match window {
                            Some(w) => {
                                pending.remove(&w);
                            }
                            // List requests fail as a whole.
                            None => {
                                if let Some(q) = queries.pop_front() {
                                    q.fail(format!("host error: {message}"));
                                }
                            }
                        }
                    }
                    HostMsg::Clipboard { text } => {
                        if cfg.clipboard_to_pc && text.len() <= umac_proto::MAX_CLIPBOARD_BYTES {
                            tracing::debug!(bytes = text.len(), "Mac clipboard received");
                            ui.send(UiEvent::Clipboard(text));
                        }
                    }
                    HostMsg::ClipboardRequest { nonce } => {
                        ctx.ids.lock().unwrap().clipboard_request(&ctx.key, nonce);
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
                let Some(cmd) = cmd else { return Ok(()) };
                match cmd {
                    NetCmd::Control(msg) => {
                        if let ClientMsg::Unsubscribe { window } = msg {
                            pending.remove(&window);
                            write_msg(&mut ctrl_send, &msg).await?;
                            for w in close_streams(ctx, streams, window, false).into_iter().filter(|&w| w != window) {
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
    }
}

#[derive(Debug, PartialEq)]
enum Show {
    /// Already shown here: bring the local window to the front.
    Raise(WindowId),
    Subscribe,
    /// Requested already; its window is on the way.
    Wait,
}

/// What a request for Mac window `mac` should do: there is one local window per Mac window.
fn plan_show(streams: &HashMap<WindowId, Stream>, pending: &HashSet<WindowId>, mac: WindowId) -> Show {
    match streams.get(&mac) {
        Some(s) => Show::Raise(s.local),
        None if pending.contains(&mac) => Show::Wait,
        None => Show::Subscribe,
    }
}

/// Asks for /Applications in Finder instead of the Apps launcher, answering `reply` when done.
async fn nonce_retry(
    ctrl_send: &mut quinn::SendStream,
    nonce: &mut u64,
    launches: &mut HashMap<u64, (LaunchAction, Reply<String>)>,
    reply: Reply<String>,
) -> Result<()> {
    *nonce += 1;
    write_msg(ctrl_send, &ClientMsg::Launch { nonce: *nonce, action: LaunchAction::FinderApplications }).await?;
    launches.insert(*nonce, (LaunchAction::FinderApplications, reply));
    Ok(())
}

/// Removes `window` and the popups and dialogs above it, forgets their local ids and, when
/// the host closed them, tells the UI. Returns the Mac ids removed.
fn close_streams(ctx: &HostCtx, streams: &mut HashMap<WindowId, Stream>, window: WindowId, notify: bool) -> Vec<WindowId> {
    let removed = remove_with_popups(streams, window);
    let mut ids = ctx.ids.lock().unwrap();
    for &w in &removed {
        if let Some(local) = ids.release(&ctx.key, w)
            && notify
        {
            ctx.ui.send(UiEvent::Closed(local));
        }
    }
    removed
}

/// Windows the host opens on its own (each kind capped). Popups need a streamed parent. New
/// windows and dialogs need `follow_new_windows` and either a streamed parent (file panels
/// come from another process) or another streamed window of the same app. With
/// `follow_focus`, any normal window the Mac focuses (an app opened from a streamed window,
/// a password prompt, an alert) is accepted too.
fn accept_unsolicited(info: &WindowInfo, streams: &HashMap<WindowId, Stream>, follow: Follow) -> bool {
    let parent_streamed = info.parent.is_some_and(|p| streams.contains_key(&p));
    let count = |kinds: &[WindowKind]| streams.values().filter(|s| s.auto && kinds.contains(&s.kind)).count();
    match info.kind {
        WindowKind::Popup => parent_streamed && count(&[WindowKind::Popup]) < MAX_POPUPS,
        WindowKind::Normal | WindowKind::Dialog => {
            let app_streamed = streams.values().any(|s| s.pid == info.pid && s.kind != WindowKind::Popup);
            let focused = follow.focus && info.kind == WindowKind::Normal && info.parent.is_none();
            ((follow.new_windows && (parent_streamed || app_streamed)) || focused)
                && count(&[WindowKind::Normal, WindowKind::Dialog]) < MAX_AUTO_WINDOWS
        }
        WindowKind::Desktop => false,
    }
}

/// New windows and dialogs followed at once.
const MAX_AUTO_WINDOWS: usize = 16;

#[derive(Debug, Clone, Copy)]
struct Follow {
    new_windows: bool,
    focus: bool,
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

/// Decodes one window on its own thread. `window` is the Mac id (for keyframe requests),
/// `local` the UI's id. A panic in the decoder closes only this window.
fn spawn_decoder(
    window: WindowId,
    local: WindowId,
    codec: Codec,
    color: ColorFormat,
    ui: UiSender,
    cmd_tx: mpsc::UnboundedSender<NetCmd>,
) -> Result<SyncSender<Arrived>> {
    let mut decoder = decode::create(codec, color)?;
    tracing::info!(window, "decoder: {}", decoder.describe());
    let (tx, rx) = sync_channel::<Arrived>(DECODE_QUEUE);
    std::thread::Builder::new().name(format!("decode-{window}")).spawn(move || {
        let run = std::panic::AssertUnwindSafe(|| {
            let slot = Arc::new(FrameSlot::default());
            let (mut n, mut total, mut since) = (0u32, Duration::ZERO, Instant::now());
            while let Ok(a) = rx.recv() {
                let t = Instant::now();
                match decoder.decode(&a.frame.data) {
                    Ok(Some(frame)) => {
                        total += t.elapsed();
                        n += 1;
                        let timing = FrameTiming { capture: a.capture, first_datagram: a.first_datagram, complete: a.complete, decoded: Instant::now() };
                        if slot.put(frame, timing) && !ui.send(UiEvent::Frame { window: local, slot: slot.clone() }) {
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
                    let ms = total.as_secs_f64() * 1e3 / n as f64;
                    tracing::info!(window, decode_ms = format_args!("{ms:.2}"), skipped = slot.take_dropped(), "decode");
                    (n, total, since) = (0, Duration::ZERO, Instant::now());
                }
            }
        });
        if std::panic::catch_unwind(run).is_err() {
            tracing::error!(window, "decoder crashed; closing the window");
            ui.send(UiEvent::Closed(local));
            let _ = cmd_tx.send(NetCmd::Control(ClientMsg::Unsubscribe { window }));
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
        let kind = if parent.is_some() { WindowKind::Popup } else { WindowKind::Normal };
        Stream {
            parent,
            pid: 1,
            kind,
            auto: parent.is_some(),
            local: 0,
            reassembler: Reassembler::default(),
            gate: KeyframeGate::new(Instant::now()),
            decode_tx: sync_channel(1).0,
            first_seen: HashMap::new(),
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
        assert!(accept_unsolicited(&popup, &streams, follow(true, false)));
        popup.parent = Some(77);
        assert!(!accept_unsolicited(&popup, &streams, follow(true, false)));
        popup.parent = None;
        assert!(!accept_unsolicited(&popup, &streams, follow(true, false)));
        assert!(!accept_unsolicited(&win(6, "Safari", WindowKind::Desktop), &streams, follow(true, false)));

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
        assert!(!accept_unsolicited(&popup, &streams, follow(true, false)));
    }

    fn follow(new_windows: bool, focus: bool) -> Follow {
        Follow { new_windows, focus }
    }

    #[test]
    fn focused_windows_are_followed_as_normal_windows_only() {
        let streams: HashMap<WindowId, Stream> = HashMap::from([(1, stream(None))]);
        let mut calc = win(5, "Calculator", WindowKind::Normal);
        calc.pid = 9;
        assert!(accept_unsolicited(&calc, &streams, follow(true, true)));
        assert!(!accept_unsolicited(&calc, &streams, follow(true, false)));
        assert!(accept_unsolicited(&calc, &streams, follow(false, true)));
        let mut dialog = win(6, "Calculator", WindowKind::Dialog);
        dialog.pid = 9;
        assert!(!accept_unsolicited(&dialog, &streams, follow(true, true)));
    }

    #[test]
    fn new_windows_and_dialogs_follow_streamed_apps() {
        let mut streams: HashMap<WindowId, Stream> = HashMap::from([(1, stream(None))]);
        let mut other_app = win(5, "Notes", WindowKind::Normal);
        other_app.pid = 2;
        // Same app (pid 1) as the streamed window: followed unless disabled.
        assert!(accept_unsolicited(&win(5, "Safari", WindowKind::Normal), &streams, follow(true, false)));
        assert!(!accept_unsolicited(&win(5, "Safari", WindowKind::Normal), &streams, follow(false, false)));
        assert!(!accept_unsolicited(&other_app, &streams, follow(true, false)));
        // A file panel comes from another process but names a streamed parent.
        let mut panel = win(6, "Open and Save Panel Service", WindowKind::Dialog);
        panel.pid = 3;
        assert!(!accept_unsolicited(&panel, &streams, follow(true, false)));
        panel.parent = Some(1);
        assert!(accept_unsolicited(&panel, &streams, follow(true, false)));
        // Capped.
        for i in 0..MAX_AUTO_WINDOWS as u32 {
            let mut s = stream(None);
            s.auto = true;
            streams.insert(200 + i, s);
        }
        assert!(!accept_unsolicited(&panel, &streams, follow(true, false)));
    }

    #[test]
    fn a_shown_window_is_raised_not_opened_twice() {
        let mut s = stream(None);
        s.local = 7;
        let streams = HashMap::from([(1, s)]);
        let pending = HashSet::from([2]);
        assert_eq!(plan_show(&streams, &pending, 1), Show::Raise(7));
        assert_eq!(plan_show(&streams, &pending, 2), Show::Wait);
        assert_eq!(plan_show(&streams, &pending, 3), Show::Subscribe);
    }

    #[test]
    fn resolves_targets() {
        let all = [win(1, "Safari", WindowKind::Normal), win(2, "Safari", WindowKind::Popup), win(3, "Notes", WindowKind::Normal)];
        assert_eq!(resolve(&[3], &["safari".into()], &all), vec![1, 3]);
        assert_eq!(resolve(&[], &["com.example.Notes".into()], &all), vec![3]);
        assert!(resolve(&[], &[], &all).is_empty());
    }
}
