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
    stream::{StreamParams, WindowStream},
    windows::{self, Geometry, OnScreen},
    ax::AxWindow,
};
use crate::{
    auth::AuthStore,
    clipboard, config,
    config::Settings,
    encode::EncodedFrame,
    input_state::Placement,
    popup::{self, Candidate},
    status::Status,
};

const SUPPORTED: [Codec; 2] = [Codec::Hevc, Codec::H264];
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
    let codec = hello.codecs.iter().copied().find(|c| SUPPORTED.contains(c)).unwrap_or(Codec::H264);
    write_msg(&mut send, &HelloReply::Accepted { host_name: shared.host_name.clone() }).await?;
    tracing::info!(client = %hello.client_name, remote = %conn.remote_address(), ?codec, "client connected");

    let (frames_tx, frames_rx) = mpsc::channel::<(WindowId, EncodedFrame)>(FRAME_QUEUE);
    let sender = tokio::spawn(send_frames(conn.clone(), frames_rx, shared.settings.fec_percent));
    static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let (reply_tx, reply_rx) = mpsc::unbounded_channel::<HostMsg>();
    let input_task = tokio::spawn(read_input(conn.clone(), shared.clone(), id, reply_tx.clone()));

    let mut session = Session {
        id,
        shared: shared.clone(),
        codec,
        streams: HashMap::new(),
        dismissed: HashSet::new(),
        frames_tx,
        replies: reply_tx,
    };
    let result = session.control_loop(&mut send, recv, reply_rx).await;

    let windows: Vec<WindowId> = session.streams.keys().copied().collect();
    drop(session);
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

async fn send_frames(conn: quinn::Connection, mut rx: mpsc::Receiver<(WindowId, EncodedFrame)>, fec_percent: u8) {
    let mut seqs: HashMap<WindowId, u32> = HashMap::new();
    while let Some((window, frame)) = rx.recv().await {
        let Some(max) = conn.max_datagram_size() else {
            tracing::error!("peer does not accept datagrams");
            return;
        };
        let seq = seqs.entry(window).or_insert(0);
        let meta = FrameMeta { window, seq: *seq, keyframe: frame.keyframe, capture_us: frame.capture_us as u32 };
        *seq = seq.wrapping_add(1);
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
    }
}

async fn read_input(conn: quinn::Connection, shared: Arc<Shared>, session: u64, reply: Reply) {
    while let Ok(mut recv) = conn.accept_uni().await {
        let (shared, reply) = (shared.clone(), reply.clone());
        tokio::spawn(async move {
            while let Ok(Some(ev)) = read_msg::<_, InputEvent>(&mut recv).await {
                match ev {
                    InputEvent::MouseButton { window, pressed: true, .. } | InputEvent::Key { window, pressed: true, .. } => {
                        *shared.active.lock().unwrap() = Some(window);
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
}

struct Session {
    id: u64,
    shared: Arc<Shared>,
    codec: Codec,
    streams: HashMap<WindowId, ActiveStream>,
    /// Popups the client closed while they are still open on the Mac; not restarted.
    dismissed: HashSet<WindowId>,
    frames_tx: mpsc::Sender<(WindowId, EncodedFrame)>,
    /// Messages from background work (input thread, launches) to this session's client.
    replies: Reply,
}

type Ctl = quinn::SendStream;

impl Session {
    async fn control_loop(
        &mut self,
        send: &mut Ctl,
        mut recv: quinn::RecvStream,
        mut replies: mpsc::UnboundedReceiver<HostMsg>,
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
        // Reading in a separate task keeps `read_msg` cancellation-safe.
        let reader = tokio::spawn(async move {
            loop {
                let r = read_msg::<_, ClientMsg>(&mut recv).await.map_err(anyhow::Error::from);
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
                }
                Some(msg) = replies.recv() => {
                    if let Err(e) = write_msg(send, &msg).await {
                        break Err(e.into());
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
            }
        };
        reader.abort();
        result
    }

    async fn on_message(&mut self, msg: ClientMsg, send: &mut Ctl) -> anyhow::Result<()> {
        match msg {
            ClientMsg::Ping { nonce } => write_msg(send, &HostMsg::Pong { nonce }).await?,
            ClientMsg::Launch { nonce, action } => {
                // Launches wait seconds for a window; run them aside so the session stays live.
                let replies = self.replies.clone();
                tokio::spawn(async move {
                    let result = match tokio::task::spawn_blocking(move || launch::run(action)).await {
                        Ok(r) => r.map_err(|e| format!("{e:#}")),
                        Err(e) => Err(format!("launch task: {e}")),
                    };
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
                let reply = match self.subscribe(window, max_fps, bitrate_kbps).await {
                    Ok(info) => HostMsg::StreamStarted { window, codec: self.codec, color: ColorFormat::Yuv420Bt709Limited, info },
                    Err(e) => HostMsg::Error { window: Some(window), message: format!("{e:#}") },
                };
                write_msg(send, &reply).await?;
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
                if self.streams.get(&window).is_some_and(|s| s.popup_of.is_some()) {
                    self.dismissed.insert(window);
                }
                self.close(window, send).await?;
            }
            ClientMsg::RequestKeyframe { window } => {
                if let Some(s) = self.streams.get(&window) {
                    s.stream.request_keyframe();
                }
            }
            ClientMsg::Focus { window } => {
                *self.shared.active.lock().unwrap() = Some(window);
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

    async fn subscribe(&mut self, window: WindowId, max_fps: u16, bitrate_kbps: u32) -> anyhow::Result<WindowInfo> {
        if let Some(s) = self.streams.get(&window) {
            s.stream.request_keyframe();
            return Ok(s.info.clone());
        }
        let params = StreamParams {
            window,
            codec: self.codec,
            fps: (max_fps as u32).clamp(1, 120),
            bitrate_kbps: self.shared.settings.clamp_bitrate(bitrate_kbps),
        };
        let tx = self.frames_tx.clone();
        let on_encoded = Arc::new(move |w: WindowId, f: EncodedFrame| tx.try_send((w, f)).is_ok());
        let (stream, sc_info, inset) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let sc = find_window(window)?;
            let app = sc.owning_application().ok_or_else(|| anyhow!("window has no owning app"))?;
            let info = (app.process_id(), app.application_name(), app.bundle_identifier(), sc.title().unwrap_or_default());
            let inset = AxWindow::find(info.0, window).map(|w| w.titlebar_inset()).unwrap_or(0.0);
            Ok((WindowStream::start(&sc, params, inset, on_encoded)?, info, inset))
        })
        .await??;
        let geometry = windows::geometry(window).ok_or_else(|| anyhow!("window {window} vanished"))?;
        let info = WindowInfo {
            id: window,
            pid: sc_info.0,
            app_name: sc_info.1,
            bundle_id: Some(sc_info.2).filter(|b| !b.is_empty()),
            title: sc_info.3,
            width: stream.size.0,
            height: stream.size.1,
            scale: stream.scale(),
            parent: None,
            offset: (0, 0),
            kind: umac_proto::WindowKind::Normal,
        };
        self.set_target(&info, &geometry, inset);
        tracing::info!(window, app = %info.app_name, size = ?stream.size, inset, ?params, "stream started");
        self.streams.insert(window, ActiveStream { stream, info: info.clone(), geometry, inset, popup_of: None, display: None });
        Ok(info)
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
            ActiveStream { stream, info: info.clone(), geometry, inset: 0.0, popup_of: None, display: Some(display_id) },
        );
        Ok(info)
    }

    fn set_target(&self, info: &WindowInfo, g: &Geometry, inset: f64) {
        if let Ok(mut t) = self.shared.targets.write() {
            let desktop = info.kind == WindowKind::Desktop;
            t.insert(info.id, Target { pid: info.pid, placement: g.placement(info.scale as f64, inset), popup: false, desktop });
        }
    }

    fn remove(&mut self, window: WindowId) {
        if self.streams.remove(&window).is_some() {
            self.shared.clip.with_focus(|f| f.window_gone(self.id, window));
            if let Ok(mut t) = self.shared.targets.write() {
                t.remove(&window);
            }
            tracing::info!(window, "stream stopped");
        }
    }

    /// Stops a stream and its popups, telling the client about every window that went away.
    async fn close(&mut self, window: WindowId, send: &mut Ctl) -> anyhow::Result<()> {
        let mut gone: Vec<WindowId> =
            self.streams.iter().filter(|(_, s)| s.popup_of == Some(window)).map(|(&id, _)| id).collect();
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
            let s = self.streams.get_mut(&id).unwrap();
            if g == s.geometry {
                continue;
            }
            let scale = s.info.scale as f64;
            let px = ((g.width * scale).round() as u32, ((g.height - s.inset).max(1.0) * scale).round() as u32);
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
            if resized || retitled {
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
            return Ok(());
        }
        let now = Instant::now();
        let list: HashMap<WindowId, OnScreen> = windows::on_screen().into_iter().map(|w| (w.id, w)).collect();
        self.dismissed.retain(|id| list.contains_key(id));

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

    async fn start_popup(&mut self, w: OnScreen, seen: Instant, send: &mut Ctl) -> anyhow::Result<()> {
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
        self.streams.insert(id, ActiveStream { stream, info: info.clone(), geometry, inset: 0.0, popup_of: Some(parent), display: None });
        write_msg(send, &HostMsg::StreamStarted { window: id, codec: self.codec, color: ColorFormat::Yuv420Bt709Limited, info })
            .await?;
        Ok(())
    }

    fn set_popup_target(&self, info: &WindowInfo, g: &Geometry) {
        if let Ok(mut t) = self.shared.targets.write() {
            let placement = Placement { x: g.x, y: g.y, width: g.width, height: g.height, scale: info.scale as f64 };
            t.insert(info.id, Target { pid: info.pid, placement, popup: true, desktop: false });
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
