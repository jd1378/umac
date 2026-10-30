//! The agent: QUIC server, per-client sessions, window streams and input routing.

use std::{
    collections::{HashMap, HashSet},
    net::{Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use tokio::{
    sync::{broadcast, mpsc, watch},
    time::timeout,
};
use umac_proto::{
    ClientMsg, Codec, ColorFormat, Cursor, Hello, HelloReply, HostMsg, InputEvent, MAX_CLIPBOARD_BYTES, PROTOCOL_VERSION,
    WindowId, WindowInfo, WindowKind, desktop_id,
    video::{PYRO_HEADER_LEN, PyroHeader, pyro_flags},
};
use umac_transport::{
    Fingerprint, Identity,
    fec::{FrameMeta, shard_frame},
    framing::{read_msg, write_msg},
    peer_fingerprint, quinn, server_endpoint,
};

use super::{
    capture::{self, Rect, display_at, find_window},
    cursor as mac_cursor,
    input::{self, ClipboardLink, Command, Reply, Target, Targets},
    launch,
    pasteboard, permissions,
    stream::{self, StreamParams, WindowStream},
    windows::{self, Geometry, OnScreen},
    ax::{self, AxWindow},
};
use crate::{
    auth::AuthStore,
    clipboard, clock, config, pyro,
    config::Settings,
    encode::EncodedFrame,
    latency::StageStats,
    input_state::Placement,
    newwin,
    owners::{self, ClientId, Owners},
    popup::{self, Candidate},
    status::Status,
};

const SUPPORTED: [Codec; 3] = [Codec::PyroWave, Codec::Hevc, Codec::H264];
/// How often the PyroWave budget is re-evaluated from loss reports.
const RATE_EVERY: Duration = Duration::from_millis(500);
/// How often per-stream latency stats are reported.
const STATS_EVERY: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(250);
/// Popups (menus, tooltips) must show up fast, so they get their own cheap, quick poll.
const POPUP_POLL: Duration = Duration::from_millis(40);
/// Encoded frames waiting for the network. Small on purpose: a backlog means stale video.
const FRAME_QUEUE: usize = 6;

struct Shared {
    settings: Settings,
    auth: AuthStore,
    targets: Targets,
    input: std_mpsc::Sender<Command>,
    /// Window that last received input or focus; new popups attach to it.
    active: Arc<Mutex<Option<WindowId>>>,
    /// New Mac clipboard text, fanned out to every session.
    clipboard: broadcast::Sender<String>,
    clip: ClipboardLink,
    /// The Mac cursor shape over streamed windows; each session forwards changes.
    cursor: watch::Receiver<Option<Arc<Cursor>>>,
    host_name: String,
    /// Which session shows each window, so a client never gets the same window twice.
    owners: Mutex<Owners>,
    /// Lets a session take a window over from another one.
    sessions: Mutex<HashMap<u64, mpsc::UnboundedSender<SessionCmd>>>,
}

/// Work one session asks of another.
enum SessionCmd {
    /// Another session of the same client now shows this window: stop it here.
    Release(WindowId),
}

pub async fn run(settings: Settings) -> anyhow::Result<()> {
    let identity = Identity::load_or_create(&config::identity_dir()).context("loading host identity")?;
    let mut status = Status {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        screen_recording: permissions::screen_recording(),
        accessibility: permissions::accessibility(),
        ..Default::default()
    };
    if !status.screen_recording || !status.accessibility {
        permissions::request_missing();
    }
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, settings.port));
    let endpoint = match server_endpoint(addr, &identity) {
        Ok(e) => e,
        Err(e) => {
            status.error = Some(format!("cannot listen on port {}: {e}", settings.port));
            status.write();
            bail!("listening on {addr}: {e}");
        }
    };
    status.listening = Some(settings.port);
    status.write();
    tracing::info!(port = settings.port, fingerprint = %identity.fingerprint(), "listening");

    // Permissions can be granted while we run; keep the status file current for `setup`.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            status.screen_recording = permissions::screen_recording();
            status.accessibility = permissions::accessibility();
            status.write();
        }
    });

    let targets = Targets::default();
    let clip = ClipboardLink::default();
    let (clipboard_tx, _) = broadcast::channel(4);
    tokio::spawn(watch_pasteboard(clip.clone(), clipboard_tx.clone()));
    let (cursor_tx, cursor_rx) = watch::channel(None);
    let cursor_kick = mac_cursor::spawn(clip.focus.clone(), cursor_tx);
    let shared = Arc::new(Shared {
        input: input::spawn(targets.clone(), clip.clone(), cursor_kick),
        cursor: cursor_rx,
        clip,
        targets,
        active: Arc::default(),
        clipboard: clipboard_tx,
        auth: AuthStore::new(&config::support_dir()),
        host_name: host_name(),
        owners: Mutex::default(),
        sessions: Mutex::default(),
        settings,
    });
    while let Some(incoming) = endpoint.accept().await {
        let shared = shared.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match handle(incoming, shared).await {
                Ok(()) => tracing::info!(%remote, "session ended"),
                Err(e) => tracing::warn!(%remote, "session error: {e:#}"),
            }
        });
    }
    Ok(())
}

/// Sends Mac copies to connected clients. umac is the only input source on a headless Mac,
/// so the pasteboard is checked shortly after injected input (when copies happen) plus a
/// slow fallback. Only the change count is polled; contents are read once per real change.
async fn watch_pasteboard(clip: ClipboardLink, tx: broadcast::Sender<String>) {
    let mut last_seen = pasteboard::change_count();
    let mut last_check = Instant::now();
    loop {
        let focused = clip.focus.lock().unwrap().any();
        let at = clipboard::next_check(*clip.last_input.lock().unwrap(), last_check, focused);
        tokio::select! {
            _ = async {
                match at {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => {}
            // New input moves the schedule; recompute it.
            _ = clip.kick.notified() => continue,
        }
        last_check = Instant::now();
        let count = pasteboard::change_count();
        let own = clip.own_write.load(std::sync::atomic::Ordering::Relaxed);
        let send = tx.receiver_count() > 0 && clipboard::should_send(count, last_seen, own);
        last_seen = count;
        if !send {
            continue;
        }
        match pasteboard::read_text() {
            Some(text) if text.len() > MAX_CLIPBOARD_BYTES => tracing::info!(bytes = text.len(), "Mac clipboard too large to sync"),
            Some(text) => {
                tracing::debug!(bytes = text.len(), "Mac clipboard changed");
                let _ = tx.send(text);
            }
            None => tracing::debug!("Mac clipboard changed (no text)"),
        }
    }
}

fn host_name() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

async fn handle(incoming: quinn::Incoming, shared: Arc<Shared>) -> anyhow::Result<()> {
    let conn = timeout(Duration::from_secs(10), incoming).await.context("handshake timeout")??;
    let fp = peer_fingerprint(&conn).ok_or_else(|| anyhow!("client sent no certificate"))?;
    let (mut send, mut recv) = timeout(Duration::from_secs(10), conn.accept_bi()).await.context("no control stream")??;
    let hello: Hello = timeout(Duration::from_secs(5), read_msg(&mut recv))
        .await
        .context("no hello")??
        .ok_or_else(|| anyhow!("stream closed before hello"))?;

    if let Err(reason) = authorize(&shared, &hello, &fp) {
        tracing::warn!(fingerprint = %fp, client = %hello.client_name, "rejected: {reason}");
        write_msg(&mut send, &HelloReply::Rejected { reason: reason.into() }).await?;
        let _ = send.finish();
        tokio::time::sleep(Duration::from_millis(200)).await;
        conn.close(1u32.into(), b"rejected");
        return Ok(());
    }
    let codec = crate::config::choose_codec(&shared.settings.codecs, &SUPPORTED, &hello.codecs);
    write_msg(&mut send, &HelloReply::Accepted { host_name: shared.host_name.clone() }).await?;
    tracing::info!(client = %hello.client_name, remote = %conn.remote_address(), ?codec, "client connected");

    let (frames_tx, frames_rx) = mpsc::channel::<(WindowId, EncodedFrame)>(FRAME_QUEUE);
    let stats = Arc::new(Mutex::new(StageStats::default()));
    let sender = tokio::spawn(send_frames(conn.clone(), frames_rx, shared.settings.fec_percent, stats.clone()));
    static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let (reply_tx, reply_rx) = mpsc::unbounded_channel::<HostMsg>();
    let client: ClientId = fp.0;
    let input_task = tokio::spawn(read_input(conn.clone(), shared.clone(), client, id, reply_tx.clone()));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    shared.sessions.lock().unwrap().insert(id, cmd_tx);
    shared.owners.lock().unwrap().open(client, id);

    let mut session = Session {
        id,
        client,
        shared: shared.clone(),
        codec,
        streams: HashMap::new(),
        dismissed: HashSet::new(),
        known: None,
        pending_new: HashMap::new(),
        focus: newwin::FocusFollower::default(),
        frames_tx,
        replies: reply_tx,
        stats,
        want_stats: false,
        rate: pyro::Rate::new(shared.settings.pyrowave_bpp),
        loss: LossCounters::default(),
        conn: conn.clone(),
    };
    let result = session.control_loop(&mut send, recv, reply_rx, cmd_rx).await;

    let windows: Vec<WindowId> = session.streams.keys().copied().collect();
    drop(session);
    shared.sessions.lock().unwrap().remove(&id);
    shared.owners.lock().unwrap().close(client, id);
    if let Ok(mut t) = shared.targets.write() {
        for w in windows {
            t.remove(&w);
        }
    }
    shared.clip.with_focus(|f| f.unfocus(id));
    let _ = shared.input.send(Command::ReleaseAll);
    input_task.abort();
    sender.abort();
    // A client closing the connection is a normal end, not an error.
    match conn.close_reason() {
        Some(quinn::ConnectionError::ApplicationClosed(_)) => Ok(()),
        _ => result,
    }
}

fn authorize(shared: &Shared, hello: &Hello, fp: &Fingerprint) -> Result<(), &'static str> {
    if hello.version != PROTOCOL_VERSION {
        return Err("protocol version mismatch");
    }
    if shared.auth.is_authorized(fp) {
        return Ok(());
    }
    match hello.pairing_token.as_deref() {
        Some(token) => match shared.auth.redeem(token, fp, &hello.client_name) {
            Ok(true) => {
                tracing::info!(fingerprint = %fp, client = %hello.client_name, "paired new client");
                Ok(())
            }
            Ok(false) => Err("invalid or expired pairing token"),
            Err(e) => {
                tracing::error!("saving paired client: {e}");
                Err("host could not store pairing")
            }
        },
        None => Err("client is not paired; run `umac-host pair` on the Mac"),
    }
}

async fn send_frames(
    conn: quinn::Connection,
    mut rx: mpsc::Receiver<(WindowId, EncodedFrame)>,
    fec_percent: u8,
    stats: Arc<Mutex<StageStats>>,
) {
    let mut seqs: HashMap<WindowId, u32> = HashMap::new();
    while let Some((window, frame)) = rx.recv().await {
        let Some(max) = conn.max_datagram_size() else {
            tracing::error!("peer does not accept datagrams");
            return;
        };
        let seq = seqs.entry(window).or_insert(0);
        let frame_seq = *seq;
        *seq = seq.wrapping_add(1);
        if let Some(pyro) = &frame.pyro {
            if !send_pyro(&conn, window, frame_seq, &frame, pyro, max).await {
                return;
            }
            let sent = clock::now_us();
            if let Ok(mut s) = stats.lock() {
                s.record(window, frame.encoded_us.saturating_sub(frame.capture_us), sent.saturating_sub(frame.encoded_us));
            }
            continue;
        }
        let meta = FrameMeta { window, seq: frame_seq, keyframe: frame.keyframe, capture_us: frame.capture_us as u32 };
        let shards = match shard_frame(meta, &frame.data, max, fec_percent) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(window, "cannot send frame: {e}");
                continue;
            }
        };
        for d in shards {
            if conn.send_datagram_wait(d.into()).await.is_err() {
                return;
            }
        }
        let sent = clock::now_us();
        if let Ok(mut s) = stats.lock() {
            s.record(window, frame.encoded_us.saturating_sub(frame.capture_us), sent.saturating_sub(frame.encoded_us));
        }
    }
}

/// One datagram per PyroWave packet, no FEC: every packet decodes on its own, so a loss
/// only blurs the blocks it carried. Returns false once the connection is gone.
async fn send_pyro(conn: &quinn::Connection, window: WindowId, frame_seq: u32, frame: &EncodedFrame, p: &crate::encode::PyroFrame, max: usize) -> bool {
    let Some(pieces) = pyro::pieces(&p.packets, max.saturating_sub(PYRO_HEADER_LEN)) else {
        tracing::warn!(window, "PyroWave frame does not fit the datagram limits; dropped");
        return true;
    };
    let packet_count = pieces.len() as u16;
    for (i, (range, continued)) in pieces.into_iter().enumerate() {
        let mut flags = if p.chroma_444 { pyro_flags::CHROMA_444 } else { 0 };
        if continued {
            flags |= pyro_flags::CONTINUED;
        }
        let header = PyroHeader {
            flags,
            window,
            frame_seq,
            capture_us: frame.capture_us as u32,
            packet_index: i as u16,
            packet_count,
            width: p.width,
            height: p.height,
        };
        let mut d = vec![0u8; PYRO_HEADER_LEN + range.len()];
        header.write((&mut d[..PYRO_HEADER_LEN]).try_into().unwrap());
        d[PYRO_HEADER_LEN..].copy_from_slice(&frame.data[range]);
        if conn.send_datagram_wait(d.into()).await.is_err() {
            return false;
        }
    }
    true
}

async fn read_input(conn: quinn::Connection, shared: Arc<Shared>, client: ClientId, session: u64, reply: Reply) {
    while let Ok(mut recv) = conn.accept_uni().await {
        let (shared, reply) = (shared.clone(), reply.clone());
        tokio::spawn(async move {
            while let Ok(Some(ev)) = read_msg::<_, InputEvent>(&mut recv).await {
                match ev {
                    InputEvent::MouseButton { window, pressed: true, .. } | InputEvent::Key { window, pressed: true, .. } => {
                        *shared.active.lock().unwrap() = Some(window);
                        shared.owners.lock().unwrap().touch(client, session);
                    }
                    // Sent when the client window loses focus.
                    InputEvent::ReleaseAll => shared.clip.with_focus(|f| f.unfocus(session)),
                    _ => {}
                }
                if shared.input.send(Command::Input(ev, reply.clone())).is_err() {
                    return;
                }
            }
        });
    }
}

struct ActiveStream {
    stream: WindowStream,
    info: WindowInfo,
    geometry: Geometry,
    /// Title bar points cropped from the capture.
    inset: f64,
    /// Set for popups: the streamed window they belong to.
    popup_of: Option<WindowId>,
    /// Set for desktop streams: the captured display (`CGDirectDisplayID`).
    display: Option<u32>,
    /// Opened by the app itself (new window, dialog), not subscribed by the client.
    auto: bool,
}

struct Session {
    id: u64,
    client: ClientId,
    shared: Arc<Shared>,
    codec: Codec,
    streams: HashMap<WindowId, ActiveStream>,
    /// Popups and followed windows the client closed while they are still open on the Mac;
    /// not restarted.
    dismissed: HashSet<WindowId>,
    /// Layer 0 windows seen by the last poll, to spot the ones streamed apps open.
    known: Option<HashSet<WindowId>>,
    /// New windows waiting for Accessibility to describe them, with when they appeared.
    pending_new: HashMap<WindowId, Instant>,
    /// Streams the Mac's focused window when it moves to one this session does not show.
    focus: newwin::FocusFollower,
    frames_tx: mpsc::Sender<(WindowId, EncodedFrame)>,
    /// Messages from background work (input thread, launches) to this session's client.
    replies: Reply,
    /// Per stream encode and send times, reported while the client wants stats.
    stats: Arc<Mutex<StageStats>>,
    want_stats: bool,
    /// PyroWave budget shared by this session's streams.
    rate: pyro::Rate,
    loss: LossCounters,
    conn: quinn::Connection,
}

/// Loss evidence for PyroWave rate control since the last evaluation.
#[derive(Default)]
struct LossCounters {
    /// Client reports (`VideoFeedback`): datagrams received and missing.
    received: u64,
    lost: u64,
    /// QUIC path counters at the last evaluation (packets sent and declared lost).
    quic_sent: u64,
    quic_lost: u64,
    last: Option<Instant>,
}

type Ctl = quinn::SendStream;

impl Session {
    async fn control_loop(
        &mut self,
        send: &mut Ctl,
        mut recv: quinn::RecvStream,
        mut replies: mpsc::UnboundedReceiver<HostMsg>,
        mut cmds: mpsc::UnboundedReceiver<SessionCmd>,
    ) -> anyhow::Result<()> {
        let mut clipboard = self.shared.clipboard.subscribe();
        let mut cursor = self.shared.cursor.clone();
        let mut cursor_sent = crate::cursor::SessionCursor::default();
        // The watcher only reports changes, so a new client starts from the current shape.
        let initial = cursor.borrow_and_update().as_ref().and_then(|c| cursor_sent.message(c));
        if let Some(msg) = initial {
            write_msg(send, &msg).await?;
        }
        let mut poll = tokio::time::interval(POLL);
        let mut popup_poll = tokio::time::interval(POPUP_POLL);
        popup_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let (msg_tx, mut msg_rx) = mpsc::channel::<anyhow::Result<Option<ClientMsg>>>(16);
        let mut stats_tick = tokio::time::interval(STATS_EVERY);
        let time_replies = self.replies.clone();
        // Reading in a separate task keeps `read_msg` cancellation-safe.
        let reader = tokio::spawn(async move {
            loop {
                let r = read_msg::<_, ClientMsg>(&mut recv).await.map_err(anyhow::Error::from);
                // Clock probes are stamped on arrival, before any queueing in this session.
                if let Ok(Some(ClientMsg::TimeProbe { nonce, client_us })) = r {
                    let _ = time_replies.send(HostMsg::TimeReply { nonce, client_us, host_us: clock::now_us() });
                    continue;
                }
                let done = !matches!(r, Ok(Some(_)));
                if msg_tx.send(r).await.is_err() || done {
                    return;
                }
            }
        });
        let result = loop {
            tokio::select! {
                msg = msg_rx.recv() => match msg {
                    Some(Ok(Some(m))) => {
                        if let Err(e) = self.on_message(m, send).await {
                            break Err(e);
                        }
                    }
                    Some(Ok(None)) | None => break Ok(()),
                    Some(Err(e)) => break Err(e),
                },
                _ = poll.tick() => {
                    if let Err(e) = self.poll_windows(send).await {
                        break Err(e);
                    }
                    self.adapt_rate();
                }
                Some(msg) = replies.recv() => {
                    if let Err(e) = write_msg(send, &msg).await {
                        break Err(e.into());
                    }
                }
                Some(SessionCmd::Release(window)) = cmds.recv() => {
                    if self.streams.contains_key(&window) {
                        tracing::info!(window, "window moved to another session of this client");
                        if let Err(e) = self.close(window, send).await {
                            break Err(e);
                        }
                    }
                }
                text = clipboard.recv() => match text {
                    Ok(text) => {
                        if let Err(e) = write_msg(send, &HostMsg::Clipboard { text }).await {
                            break Err(e.into());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {}
                },
                Ok(()) = cursor.changed() => {
                    let msg = cursor.borrow_and_update().as_ref().and_then(|c| cursor_sent.message(c));
                    if let Some(msg) = msg
                        && let Err(e) = write_msg(send, &msg).await
                    {
                        break Err(e.into());
                    }
                }
                _ = popup_poll.tick() => {
                    if let Err(e) = self.poll_popups(send).await {
                        break Err(e);
                    }
                }
                _ = stats_tick.tick() => {
                    let msgs = self.stats.lock().map(|mut s| s.drain()).unwrap_or_default();
                    let mut failed = None;
                    for msg in msgs.iter().filter(|_| self.want_stats) {
                        if let Err(e) = write_msg(send, msg).await {
                            failed = Some(e);
                            break;
                        }
                    }
                    if let Some(e) = failed {
                        break Err(e.into());
                    }
                }
            }
        };
        reader.abort();
        result
    }

    /// Re-evaluates the PyroWave budget from client loss reports and QUIC's own loss
    /// counters (whichever shows more loss), and refreshes the packet size from the path MTU.
    fn adapt_rate(&mut self) {
        if self.codec != Codec::PyroWave {
            return;
        }
        let now = Instant::now();
        if self.loss.last.is_some_and(|t| now.duration_since(t) < RATE_EVERY) {
            return;
        }
        let path = self.conn.stats().path;
        let (sent, lost) = (path.sent_packets - self.loss.quic_sent.min(path.sent_packets), path.lost_packets - self.loss.quic_lost.min(path.lost_packets));
        let quic_ratio = if sent > 0 { lost as f64 / sent as f64 } else { 0.0 };
        let client_total = self.loss.received + self.loss.lost;
        let client_ratio = if client_total > 0 { self.loss.lost as f64 / client_total as f64 } else { 0.0 };
        let (received, lost) = if client_ratio >= quic_ratio { (self.loss.received, self.loss.lost) } else { (sent - lost, lost) };
        let before = self.rate.bpp();
        if self.loss.last.is_some() {
            self.rate.report(received, lost, now);
        }
        self.loss = LossCounters { quic_sent: path.sent_packets, quic_lost: path.lost_packets, last: Some(now), ..Default::default() };
        if (self.rate.bpp() - before).abs() > 1e-6 {
            tracing::info!(bpp = format_args!("{:.2}", self.rate.bpp()), quic_loss = format_args!("{quic_ratio:.3}"), client_loss = format_args!("{client_ratio:.3}"), "PyroWave budget changed");
        }
        let bpp_milli = (self.rate.bpp() * 1000.0) as u32;
        let boundary = self.conn.max_datagram_size().map(|m| stream::packet_boundary(m as u32));
        for (&window, s) in &self.streams {
            let k = s.stream.knobs();
            let mut times = k.encode_us.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
            if times.len() >= 300 {
                times.sort_unstable();
                let pct = |p: f64| times[((times.len() - 1) as f64 * p) as usize] as f64 / 1000.0;
                let split = k.oversized.swap(0, Ordering::Relaxed);
                tracing::info!(window, frames = times.len(), p50 = format_args!("{:.2}", pct(0.5)), p95 = format_args!("{:.2}", pct(0.95)), split, "PyroWave encode ms");
            } else if let Ok(mut v) = k.encode_us.lock() {
                v.extend(times);
            }
            k.bpp_milli.store(bpp_milli, Ordering::Relaxed);
            if let Some(b) = boundary {
                k.packet_boundary.store(b, Ordering::Relaxed);
            }
        }
    }

    /// Initial PyroWave knobs for a new stream.
    fn pyro_params(&self) -> (u32, u32) {
        ((self.rate.bpp() * 1000.0) as u32, self.conn.max_datagram_size().unwrap_or(1200) as u32)
    }

    async fn on_message(&mut self, msg: ClientMsg, send: &mut Ctl) -> anyhow::Result<()> {
        match msg {
            ClientMsg::Ping { nonce } => write_msg(send, &HostMsg::Pong { nonce }).await?,
            ClientMsg::WantStats { on } => self.want_stats = on,
            ClientMsg::VideoFeedback { received, lost, .. } => {
                self.loss.received += received as u64;
                self.loss.lost += lost as u64;
            }
            // Answered by the reader task as soon as it arrives.
            ClientMsg::TimeProbe { .. } => {}
            ClientMsg::Launch { nonce, action } => {
                self.touch();
                // Launches wait seconds for a window; run them aside so the session stays live.
                let (replies, shared, client) = (self.replies.clone(), self.shared.clone(), self.client);
                tokio::spawn(async move {
                    let result = match tokio::task::spawn_blocking(move || launch::run(action)).await {
                        Ok(r) => r.map_err(|e| format!("{e:#}")),
                        Err(e) => Err(format!("launch task: {e}")),
                    };
                    if let Ok(info) = &result {
                        shared.owners.lock().unwrap().hold(client, info.id, Instant::now() + owners::LAUNCH_HOLD);
                    }
                    if let Err(e) = &result {
                        tracing::warn!(?action, "launch failed: {e}");
                    }
                    let _ = replies.send(HostMsg::Launched { nonce, result });
                });
            }
            ClientMsg::ListWindows => {
                let reply = match tokio::task::spawn_blocking(windows::list).await? {
                    Ok(list) => HostMsg::Windows(list),
                    Err(e) => HostMsg::Error { window: None, message: format!("{e:#}") },
                };
                write_msg(send, &reply).await?;
            }
            ClientMsg::Subscribe { window, max_fps, bitrate_kbps } => {
                self.touch();
                // On success StreamStarted was already sent, before capture started.
                if let Err(e) = self.subscribe(window, max_fps, bitrate_kbps, send).await {
                    write_msg(send, &HostMsg::Error { window: Some(window), message: format!("{e:#}") }).await?;
                }
            }
            ClientMsg::ListDisplays => {
                let reply = match tokio::task::spawn_blocking(capture::displays).await? {
                    Ok(list) => HostMsg::Displays(windows::display_infos(&list)),
                    Err(e) => HostMsg::Error { window: None, message: format!("{e:#}") },
                };
                write_msg(send, &reply).await?;
            }
            ClientMsg::SubscribeDisplay { index, max_fps, bitrate_kbps } => {
                let window = desktop_id(index);
                let reply = match self.subscribe_display(index, max_fps, bitrate_kbps).await {
                    Ok(info) => HostMsg::StreamStarted { window, codec: self.codec, color: ColorFormat::Yuv420Bt709Limited, info },
                    Err(e) => HostMsg::Error { window: Some(window), message: format!("{e:#}") },
                };
                write_msg(send, &reply).await?;
            }
            ClientMsg::Unsubscribe { window } => {
                // Closing a popup on the client does not dismiss it on the Mac: a synthetic
                // Escape could land in another window if the menu already closed itself. The
                // click that dismissed it on the client reaches the Mac and closes it there.
                if self.streams.get(&window).is_some_and(|s| s.popup_of.is_some() || s.auto) {
                    self.dismissed.insert(window);
                }
                self.focus.dismissed(window);
                self.close(window, send).await?;
            }
            ClientMsg::CloseWindow { window } => {
                self.touch();
                let Some(s) = self.streams.get(&window) else { return Ok(()) };
                let target = (s.popup_of.is_none() && s.display.is_none()).then_some(s.info.pid);
                let pressed = match target {
                    Some(pid) => tokio::task::spawn_blocking(move || AxWindow::find(pid, window)?.close()).await?,
                    None => Err(anyhow!("popups and desktops are only unsubscribed")),
                };
                match pressed {
                    // The stream ends when the window is gone (WindowClosed from the poll). If the
                    // app asks to save first, that prompt is followed as a dialog of this window.
                    Ok(()) => tracing::info!(window, "closing the window on the Mac for the client"),
                    Err(e) => {
                        tracing::info!(window, "not closing on the Mac ({e:#}); unsubscribing");
                        if self.streams.get(&window).is_some_and(|s| s.popup_of.is_some() || s.auto) {
                            self.dismissed.insert(window);
                        }
                        self.focus.dismissed(window);
                        self.close(window, send).await?;
                    }
                }
            }
            ClientMsg::RequestKeyframe { window } => {
                if let Some(s) = self.streams.get(&window) {
                    s.stream.request_keyframe();
                }
            }
            ClientMsg::Focus { window } => {
                *self.shared.active.lock().unwrap() = Some(window);
                self.touch();
                let Some(s) = self.streams.get(&window) else { return Ok(()) };
                if s.popup_of.is_none() {
                    // A desktop has no window to raise; the user picks windows on the Mac.
                    if s.display.is_none() {
                        let _ = self.shared.input.send(Command::Focus(window));
                    }
                    self.shared.clip.with_focus(|f| f.focus(self.id, window));
                }
            }
            ClientMsg::Resize { window, width, height } => {
                let Some(s) = self.streams.get(&window) else { return Ok(()) };
                // Popups follow the Mac; a desktop is scaled on the client instead.
                if s.popup_of.is_some() || s.display.is_some() {
                    return Ok(());
                }
                let (pid, scale) = (s.info.pid, s.info.scale.max(1.0) as f64);
                let (w, h) = (width as f64 / scale, height as f64 / scale + s.inset);
                let res = tokio::task::spawn_blocking(move || AxWindow::find(pid, window)?.resize(w, h)).await?;
                match res {
                    Ok(got) => tracing::debug!(window, requested = ?(w, h), ?got, "resized"),
                    Err(e) => write_msg(send, &HostMsg::Error { window: Some(window), message: format!("resize: {e:#}") }).await?,
                }
            }
        }
        Ok(())
    }

    async fn subscribe(&mut self, window: WindowId, max_fps: u16, bitrate_kbps: u32, send: &mut Ctl) -> anyhow::Result<()> {
        if let Some(s) = self.streams.get(&window) {
            s.stream.request_keyframe();
            let info = s.info.clone();
            write_msg(send, &HostMsg::StreamStarted { window, codec: self.codec, color: ColorFormat::Yuv420Bt709Limited, info }).await?;
            return Ok(());
        }
        let params = StreamParams {
            window,
            codec: self.codec,
            fps: (max_fps as u32).clamp(1, 120),
            bitrate_kbps: self.shared.settings.clamp_bitrate(bitrate_kbps),
            pyro_bpp_milli: self.pyro_params().0,
            max_datagram: self.pyro_params().1,
        };
        self.start_window(params, WindowKind::Normal, None, false, send).await.map(|_| ())
    }

    /// This session is the one the user works in now (it follows the Mac's focus).
    fn touch(&self) {
        self.shared.owners.lock().unwrap().touch(self.client, self.id);
    }

    /// Another session of this client already shows `window`.
    fn taken_elsewhere(&self, window: WindowId) -> bool {
        self.shared.owners.lock().unwrap().taken_elsewhere(self.client, window, self.id)
    }

    /// Takes `window` for this session, making any other session of this client drop it.
    fn claim(&self, window: WindowId) {
        let old = self.shared.owners.lock().unwrap().claim(self.client, window, self.id);
        if let Some(old) = old
            && let Some(tx) = self.shared.sessions.lock().unwrap().get(&old)
        {
            let _ = tx.send(SessionCmd::Release(window));
        }
    }

    /// Captures one Mac window with its title bar cropped. `parent` places dialogs. The client
    /// is told as soon as the window is known (its window opens black), and capture starts
    /// after; if that fails the window is closed again.
    async fn start_window(
        &mut self,
        params: StreamParams,
        kind: WindowKind,
        parent: Option<WindowId>,
        auto: bool,
        send: &mut Ctl,
    ) -> anyhow::Result<WindowInfo> {
        let window = params.window;
        let started = Instant::now();
        let (sc, app, inset, (scale, size)) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let sc = find_window(window)?;
            let app = sc.owning_application().ok_or_else(|| anyhow!("window has no owning app"))?;
            let app = (app.process_id(), app.application_name(), app.bundle_identifier(), sc.title().unwrap_or_default());
            let inset = AxWindow::find(app.0, window).map(|w| w.titlebar_inset()).unwrap_or(0.0);
            let size = capture::window_size(&sc, inset)?;
            Ok((sc, app, inset, size))
        })
        .await??;
        let geometry = windows::geometry(window).ok_or_else(|| anyhow!("window {window} vanished"))?;
        let offset = parent.and_then(|p| self.dialog_offset(p, &geometry, inset)).unwrap_or((0, 0));
        let mut info = WindowInfo {
            id: window,
            pid: app.0,
            app_name: app.1,
            bundle_id: Some(app.2).filter(|b| !b.is_empty()),
            title: app.3,
            width: size.0,
            height: size.1,
            scale,
            parent,
            offset,
            kind,
        };
        self.claim(window);
        write_msg(send, &HostMsg::StreamStarted { window, codec: self.codec, color: ColorFormat::Yuv420Bt709Limited, info: info.clone() })
            .await?;
        let announced_ms = started.elapsed().as_millis() as u64;

        let tx = self.frames_tx.clone();
        let first = AtomicBool::new(false);
        let on_encoded = Arc::new(move |w: WindowId, f: EncodedFrame| {
            if !first.swap(true, Ordering::Relaxed) {
                tracing::info!(window = w, ms = started.elapsed().as_millis() as u64, "first frame encoded");
            }
            tx.try_send((w, f)).is_ok()
        });
        let started_capture = tokio::task::spawn_blocking(move || WindowStream::start(&sc, params, inset, on_encoded))
            .await
            .map_err(|e| anyhow!("capture task: {e}"))
            .and_then(|r| r);
        let stream = match started_capture {
            Ok(stream) => stream,
            Err(e) => {
                self.shared.owners.lock().unwrap().release(self.client, window, self.id);
                write_msg(send, &HostMsg::WindowClosed { window }).await?;
                return Err(e.context("starting capture"));
            }
        };
        if stream.size != size {
            (info.width, info.height) = stream.size;
            write_msg(send, &HostMsg::WindowChanged(info.clone())).await?;
        }
        self.set_target(&info, &geometry, inset);
        tracing::info!(
            window,
            app = %info.app_name,
            ?kind,
            ?parent,
            auto,
            size = ?stream.size,
            inset,
            announced_ms,
            capture_ms = started.elapsed().as_millis() as u64,
            ?params,
            "stream started"
        );
        self.streams.insert(window, ActiveStream { stream, info: info.clone(), geometry, inset, popup_of: None, display: None, auto });
        Ok(info)
    }

    /// Dialog content origin relative to its parent's surface, in parent surface pixels.
    fn dialog_offset(&self, parent: WindowId, g: &Geometry, inset: f64) -> Option<(i32, i32)> {
        let (origin, scale) = self.parent_origin(parent)?;
        Some(popup::offset((g.x, g.y + inset), origin, scale))
    }

    /// Follows a window the app opened by itself, once Accessibility can describe it.
    /// Returns false while it should be retried on a later poll.
    async fn follow_new(&mut self, w: OnScreen, seen: Instant, pids: &HashSet<i32>, send: &mut Ctl) -> anyhow::Result<bool> {
        if w.width < newwin::MIN_SIZE || w.height < newwin::MIN_SIZE || self.dismissed.contains(&w.id) || self.taken_elsewhere(w.id) {
            return Ok(true);
        }
        let panel = !pids.contains(&w.pid);
        if panel && !tokio::task::spawn_blocking(move || windows::owner_name(w.id)).await?.is_some_and(|n| newwin::is_panel_service(&n)) {
            return Ok(true);
        }
        if self.streams.values().filter(|s| s.auto).count() >= newwin::MAX_AUTO {
            tracing::info!(window = w.id, "not following new window: limit reached");
            return Ok(true);
        }
        let (pid, id) = (w.pid, w.id);
        let ax = tokio::task::spawn_blocking(move || ax::window_kind(pid, id)).await?;
        let late = seen.elapsed().as_millis() as u64 >= newwin::CLASSIFY_GRACE_MS;
        let kind = match (&ax, panel) {
            (Some(ax), false) => newwin::classify(ax),
            (_, true) => WindowKind::Dialog,
            // Popovers and other transient panels have no AX window; keep them with the app.
            (None, false) if late => WindowKind::Dialog,
            (None, false) => return Ok(false),
        };
        let parent = match kind {
            WindowKind::Dialog => {
                let parent = if panel { self.active_root() } else { self.popup_parent(pid) };
                Some(parent.ok_or_else(|| anyhow!("no parent for dialog {id}"))?)
            }
            _ => None,
        };
        let template = parent
            .and_then(|p| self.streams.get(&p))
            .or_else(|| self.streams.values().find(|s| s.info.pid == pid && s.popup_of.is_none() && s.display.is_none()))
            .map(|s| s.stream.params())
            .ok_or_else(|| anyhow!("no stream to copy settings from"))?;
        self.start_window(StreamParams { window: id, ..template }, kind, parent, true, send).await?;
        tracing::info!(window = id, ?kind, ?parent, ?ax, ms = seen.elapsed().as_millis() as u64, "following new window");
        Ok(true)
    }

    /// Streams the Mac's focused window when focus moves to one this session does not show:
    /// an app opened from a streamed window, an alert or password prompt that pops up by
    /// itself, a window brought forward with Cmd+Tab.
    async fn follow_focus(
        &mut self,
        ordered: &[OnScreen],
        list: &HashMap<WindowId, OnScreen>,
        streamed_pids: &HashSet<i32>,
        send: &mut Ctl,
    ) -> anyhow::Result<()> {
        if self.streams.values().any(|s| s.display.is_some()) {
            return Ok(());
        }
        // Only the client's most recently active session opens newly focused windows.
        let follows = self.shared.owners.lock().unwrap().follows_focus(self.client, self.id);
        let windows: Vec<(WindowId, i32, i32, f64, f64)> = ordered.iter().map(|w| (w.id, w.pid, w.layer, w.width, w.height)).collect();
        let focused = tokio::task::spawn_blocking(ax::focused_window)
            .await?
            .and_then(|(pid, id)| id.filter(|id| list.contains_key(id)).or_else(|| newwin::front_window(pid, &windows)));
        let own = std::process::id() as i32;
        let candidate = {
            let streams = &self.streams;
            let owners = self.shared.owners.lock().unwrap();
            let shown = |id: WindowId| streams.contains_key(&id) || owners.taken_elsewhere(self.client, id, self.id);
            self.focus.update(focused, shown, |id| {
                list.get(&id).is_some_and(|w| w.pid != own && newwin::focus_eligible(w.layer, w.width, w.height))
            })
        };
        // Other sessions keep their follower state current but leave the opening to the active one.
        let Some(id) = candidate.filter(|_| follows) else { return Ok(()) };
        let w = list[&id];
        let owner = tokio::task::spawn_blocking(move || windows::owner_name(id)).await?.unwrap_or_default();
        if newwin::is_system_ui(&owner) {
            self.focus.failed();
            return Ok(());
        }
        if self.streams.values().filter(|s| s.auto).count() >= newwin::MAX_AUTO {
            tracing::info!(window = id, "not following the focused window: limit reached");
            self.focus.failed();
            return Ok(());
        }
        // A window of an app already shown keeps the usual rules (dialogs stay with it).
        let kind = if streamed_pids.contains(&w.pid) {
            let pid = w.pid;
            tokio::task::spawn_blocking(move || ax::window_kind(pid, id)).await?.map_or(WindowKind::Normal, |ax| newwin::classify(&ax))
        } else {
            WindowKind::Normal
        };
        let parent = match kind {
            WindowKind::Dialog => self.popup_parent(w.pid),
            _ => None,
        };
        let kind = if parent.is_some() { kind } else { WindowKind::Normal };
        let template = self
            .streams
            .values()
            .find(|s| s.popup_of.is_none() && s.display.is_none())
            .map(|s| s.stream.params())
            .ok_or_else(|| anyhow!("no stream to copy settings from"))?;
        let info = self.start_window(StreamParams { window: id, ..template }, kind, parent, true, send).await?;
        tracing::info!(app = %info.app_name, pid = w.pid, window = id, ?kind, ?parent, "following the focused window");
        Ok(())
    }

    /// The streamed window the user works in (a popup resolves to its parent), for file panels
    /// that a separate process shows on behalf of the app.
    fn active_root(&self) -> Option<WindowId> {
        let usable = |id: &WindowId| self.streams.get(id).is_some_and(|s| s.display.is_none() && s.popup_of.is_none());
        let active = *self.shared.active.lock().unwrap();
        let active = active.map(|a| self.streams.get(&a).and_then(|s| s.popup_of).unwrap_or(a));
        active.filter(usable).or_else(|| self.streams.keys().copied().filter(usable).min())
    }

    async fn subscribe_display(&mut self, index: u32, max_fps: u16, bitrate_kbps: u32) -> anyhow::Result<WindowInfo> {
        let window = desktop_id(index);
        if let Some(s) = self.streams.get(&window) {
            s.stream.request_keyframe();
            return Ok(s.info.clone());
        }
        let params = StreamParams {
            window,
            codec: self.codec,
            fps: (max_fps as u32).clamp(1, 120),
            bitrate_kbps: self.shared.settings.clamp_bitrate(bitrate_kbps),
            pyro_bpp_milli: self.pyro_params().0,
            max_datagram: self.pyro_params().1,
        };
        let tx = self.frames_tx.clone();
        let on_encoded = Arc::new(move |w: WindowId, f: EncodedFrame| tx.try_send((w, f)).is_ok());
        let (stream, display_id, name) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let list = capture::displays()?;
            let d = list.get(index as usize).ok_or_else(|| anyhow!("no display {index}"))?;
            let name = windows::display_infos(&list).swap_remove(index as usize).name;
            Ok((WindowStream::start_display(d, params, on_encoded)?, d.display_id(), name))
        })
        .await??;
        let (x, y, width, height) = windows::display_bounds(display_id);
        let geometry = Geometry { x, y, width, height, title: name.clone(), on_screen: true };
        let info = WindowInfo {
            id: window,
            pid: 0,
            app_name: "Desktop".into(),
            bundle_id: None,
            title: name,
            width: stream.size.0,
            height: stream.size.1,
            scale: stream.scale(),
            parent: None,
            offset: (0, 0),
            kind: WindowKind::Desktop,
        };
        self.set_target(&info, &geometry, 0.0);
        tracing::info!(window, display = display_id, size = ?stream.size, ?params, "desktop stream started");
        self.streams.insert(
            window,
            ActiveStream { stream, info: info.clone(), geometry, inset: 0.0, popup_of: None, display: Some(display_id), auto: false },
        );
        Ok(info)
    }

    fn set_target(&self, info: &WindowInfo, g: &Geometry, inset: f64) {
        if let Ok(mut t) = self.shared.targets.write() {
            let desktop = info.kind == WindowKind::Desktop;
            let parent_pid = info.parent.and_then(|p| self.streams.get(&p)).map(|s| s.info.pid).filter(|&p| p != info.pid);
            t.insert(info.id, Target { pid: info.pid, placement: g.placement(info.scale as f64, inset), popup: false, desktop, parent_pid });
        }
    }

    fn remove(&mut self, window: WindowId) {
        if self.streams.remove(&window).is_some() {
            self.shared.owners.lock().unwrap().release(self.client, window, self.id);
            self.shared.clip.with_focus(|f| f.window_gone(self.id, window));
            if let Ok(mut t) = self.shared.targets.write() {
                t.remove(&window);
            }
            tracing::info!(window, "stream stopped");
        }
    }

    /// Stops a stream and its popups, telling the client about every window that went away.
    async fn close(&mut self, window: WindowId, send: &mut Ctl) -> anyhow::Result<()> {
        let mut gone: Vec<WindowId> = self
            .streams
            .iter()
            .filter(|(_, s)| s.popup_of == Some(window) || (s.auto && s.info.parent == Some(window)))
            .map(|(&id, _)| id)
            .collect();
        gone.push(window);
        for id in gone {
            if self.streams.contains_key(&id) {
                self.remove(id);
                write_msg(send, &HostMsg::WindowClosed { window: id }).await?;
            }
        }
        Ok(())
    }

    async fn poll_windows(&mut self, send: &mut Ctl) -> anyhow::Result<()> {
        let ids: Vec<WindowId> = self.streams.keys().copied().collect();
        for id in ids {
            let Some(s) = self.streams.get(&id) else { continue };
            s.stream.serve_idle_keyframe();
            // Popups are tracked by the faster popup poll.
            if s.popup_of.is_some() {
                continue;
            }
            if let Some(display) = s.display {
                self.poll_display(id, display, send).await?;
                continue;
            }
            let Some(g) = windows::geometry(id) else {
                self.close(id, send).await?;
                continue;
            };
            let s = &self.streams[&id];
            // Dialogs and sheets move with their parent, which changes the offset too.
            let offset = s.info.parent.and_then(|p| self.dialog_offset(p, &g, s.inset)).unwrap_or(s.info.offset);
            let s = self.streams.get_mut(&id).unwrap();
            let moved = offset != s.info.offset;
            s.info.offset = offset;
            if g == s.geometry {
                if moved {
                    write_msg(send, &HostMsg::WindowChanged(s.info.clone())).await?;
                }
                continue;
            }
            let scale = s.info.scale as f64;
            let px = (capture::pixels(g.width, scale), capture::pixels((g.height - s.inset).max(1.0), scale));
            let resized = px != (s.info.width, s.info.height);
            if resized {
                if let Err(e) = s.stream.resize(px.0, px.1) {
                    tracing::warn!(window = id, "resizing capture: {e:#}");
                }
                s.info.width = px.0;
                s.info.height = px.1;
            }
            let retitled = g.title != s.geometry.title && !g.title.is_empty();
            if retitled {
                s.info.title = g.title.clone();
            }
            s.geometry = g;
            let (info, geometry, inset) = (s.info.clone(), s.geometry.clone(), s.inset);
            self.set_target(&info, &geometry, inset);
            if resized || retitled || moved {
                write_msg(send, &HostMsg::WindowChanged(info)).await?;
            }
        }
        Ok(())
    }

    /// Follows a display resolution change; scale changes need a new stream (not handled).
    async fn poll_display(&mut self, id: WindowId, display: u32, send: &mut Ctl) -> anyhow::Result<()> {
        let (x, y, width, height) = windows::display_bounds(display);
        let s = self.streams.get_mut(&id).unwrap();
        if (x, y, width, height) == (s.geometry.x, s.geometry.y, s.geometry.width, s.geometry.height) {
            return Ok(());
        }
        if width <= 0.0 || height <= 0.0 {
            return self.close(id, send).await;
        }
        if let Err(e) = s.stream.set_region(Rect { x, y, width, height }) {
            tracing::warn!(window = id, "resizing desktop capture: {e:#}");
            return Ok(());
        }
        (s.geometry.x, s.geometry.y, s.geometry.width, s.geometry.height) = (x, y, width, height);
        (s.info.width, s.info.height) = s.stream.size;
        let (info, geometry) = (s.info.clone(), s.geometry.clone());
        self.set_target(&info, &geometry, 0.0);
        write_msg(send, &HostMsg::WindowChanged(info)).await?;
        Ok(())
    }

    /// Finds popups opened by streamed apps, follows their geometry and closes them when
    /// they disappear.
    async fn poll_popups(&mut self, send: &mut Ctl) -> anyhow::Result<()> {
        // Desktop streams show popups as part of the display already.
        let pids: HashSet<i32> =
            self.streams.values().filter(|s| s.popup_of.is_none() && s.display.is_none()).map(|s| s.info.pid).collect();
        if pids.is_empty() && self.dismissed.is_empty() {
            // Nothing to follow: start over so windows opened meanwhile do not count as new.
            self.known = None;
            self.pending_new.clear();
            return Ok(());
        }
        let now = Instant::now();
        let ordered = windows::on_screen();
        let list: HashMap<WindowId, OnScreen> = ordered.iter().map(|w| (w.id, *w)).collect();
        self.dismissed.retain(|id| list.contains_key(id));

        let snapshot: Vec<(WindowId, i32)> = list.values().map(|w| (w.id, w.layer)).collect();
        for id in newwin::fresh(&mut self.known, &snapshot) {
            self.pending_new.insert(id, now);
        }
        self.pending_new.retain(|id, _| list.contains_key(id) && !self.streams.contains_key(id));
        if !pids.is_empty() {
            let pending: Vec<(WindowId, Instant)> = self.pending_new.iter().map(|(&id, &t)| (id, t)).collect();
            for (id, seen) in pending {
                let done = match self.follow_new(list[&id], seen, &pids, send).await {
                    Ok(done) => done,
                    Err(e) => {
                        tracing::debug!(window = id, "not following new window: {e:#}");
                        true
                    }
                };
                if done {
                    self.pending_new.remove(&id);
                }
            }
            if let Err(e) = self.follow_focus(&ordered, &list, &pids, send).await {
                tracing::debug!("not following the focused window: {e:#}");
                self.focus.failed();
            }
        }

        let popups: Vec<WindowId> = self.streams.iter().filter(|(_, s)| s.popup_of.is_some()).map(|(&id, _)| id).collect();
        for id in popups {
            match list.get(&id) {
                Some(w) => self.update_popup(*w, send).await?,
                None => self.close(id, send).await?,
            }
        }
        for w in list.values() {
            if !pids.contains(&w.pid) || self.streams.contains_key(&w.id) || self.dismissed.contains(&w.id) {
                continue;
            }
            if !popup::is_popup(w.layer, w.width, w.height) {
                continue;
            }
            if let Err(e) = self.start_popup(*w, now, send).await {
                tracing::debug!(window = w.id, layer = w.layer, "popup not started yet: {e:#}");
            }
        }
        Ok(())
    }

    fn popup_parent(&self, pid: i32) -> Option<WindowId> {
        let candidates: Vec<Candidate> =
            self.streams
                .values()
                .filter(|s| s.display.is_none())
                .map(|s| Candidate { id: s.info.id, pid: s.info.pid, popup_of: s.popup_of })
                .collect();
        popup::choose_parent(pid, *self.shared.active.lock().unwrap(), &candidates)
    }

    /// Parent capture origin in global points and its surface scale.
    fn parent_origin(&self, parent: WindowId) -> Option<((f64, f64), f64)> {
        let p = self.streams.get(&parent)?;
        Some(((p.geometry.x, p.geometry.y + p.inset), p.info.scale as f64))
    }

    /// Popups are announced only once capture runs: a menu that flashes black first looks
    /// worse than one that appears some 60 ms later.
    async fn start_popup(&mut self, w: OnScreen, seen: Instant, send: &mut Ctl) -> anyhow::Result<()> {
        if self.taken_elsewhere(w.id) {
            return Ok(());
        }
        let parent = self.popup_parent(w.pid).ok_or_else(|| anyhow!("no parent window"))?;
        let (origin, scale) = self.parent_origin(parent).ok_or_else(|| anyhow!("parent gone"))?;
        let p = &self.streams[&parent];
        let params = StreamParams { window: w.id, ..p.stream.params() };
        let mut info = WindowInfo {
            id: w.id,
            pid: w.pid,
            app_name: p.info.app_name.clone(),
            bundle_id: p.info.bundle_id.clone(),
            title: String::new(),
            width: 0,
            height: 0,
            scale: p.info.scale,
            parent: Some(parent),
            offset: popup::offset((w.x, w.y), origin, scale),
            kind: WindowKind::Popup,
        };
        let tx = self.frames_tx.clone();
        let first = AtomicBool::new(false);
        let on_encoded = Arc::new(move |id: WindowId, f: EncodedFrame| {
            if !first.swap(true, Ordering::Relaxed) {
                tracing::info!(window = id, ms = seen.elapsed().as_millis() as u64, "popup first frame");
            }
            tx.try_send((id, f)).is_ok()
        });
        let (id, rect) = (w.id, Rect { x: w.x, y: w.y, width: w.width, height: w.height });
        let (stream, find_ms) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let t = Instant::now();
            let display = display_at(rect.x, rect.y)?;
            let find_ms = t.elapsed().as_millis() as u64;
            Ok((WindowStream::start_region(&display, rect, params, on_encoded)?, find_ms))
        })
        .await??;
        (info.width, info.height) = stream.size;
        tracing::info!(
            window = id, parent, layer = w.layer, offset = ?info.offset, size = ?stream.size,
            find_ms, start_ms = seen.elapsed().as_millis() as u64, "popup started"
        );
        if tracing::enabled!(tracing::Level::DEBUG) {
            let center = (w.x + w.width / 2.0, w.y + w.height / 2.0);
            tokio::task::spawn_blocking(move || super::ax::log_menu_items(center.0, center.1));
        }
        let geometry = Geometry { x: w.x, y: w.y, width: w.width, height: w.height, title: String::new(), on_screen: true };
        self.set_popup_target(&info, &geometry);
        self.claim(id);
        self.streams.insert(id, ActiveStream { stream, info: info.clone(), geometry, inset: 0.0, popup_of: Some(parent), display: None, auto: false });
        write_msg(send, &HostMsg::StreamStarted { window: id, codec: self.codec, color: ColorFormat::Yuv420Bt709Limited, info })
            .await?;
        Ok(())
    }

    fn set_popup_target(&self, info: &WindowInfo, g: &Geometry) {
        if let Ok(mut t) = self.shared.targets.write() {
            let placement = Placement { x: g.x, y: g.y, width: g.width, height: g.height, scale: info.scale as f64 };
            t.insert(info.id, Target { pid: info.pid, placement, popup: true, desktop: false, parent_pid: None });
        }
    }

    /// Follows popup moves and resizes, and parent moves that shift the offset.
    async fn update_popup(&mut self, w: OnScreen, send: &mut Ctl) -> anyhow::Result<()> {
        let Some(parent) = self.streams.get(&w.id).and_then(|s| s.popup_of) else { return Ok(()) };
        let Some((origin, scale)) = self.parent_origin(parent) else { return Ok(()) };
        let s = self.streams.get_mut(&w.id).unwrap();
        let offset = popup::offset((w.x, w.y), origin, scale);
        let moved = (w.x, w.y, w.width, w.height) != (s.geometry.x, s.geometry.y, s.geometry.width, s.geometry.height);
        if !moved && offset == s.info.offset {
            return Ok(());
        }
        if moved {
            if let Err(e) = s.stream.set_region(Rect { x: w.x, y: w.y, width: w.width, height: w.height }) {
                tracing::warn!(window = w.id, "moving popup capture: {e:#}");
            }
            (s.info.width, s.info.height) = s.stream.size;
        }
        (s.geometry.x, s.geometry.y, s.geometry.width, s.geometry.height) = (w.x, w.y, w.width, w.height);
        s.info.offset = offset;
        let (info, geometry) = (s.info.clone(), s.geometry.clone());
        self.set_popup_target(&info, &geometry);
        Ok(write_msg(send, &HostMsg::WindowChanged(info)).await?)
    }
}
