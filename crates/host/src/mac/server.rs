//! The agent: QUIC server, per-client sessions, window streams and input routing.

use std::{
    collections::HashMap,
    net::{Ipv6Addr, SocketAddr},
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use tokio::{sync::mpsc, time::timeout};
use umac_proto::{
    ClientMsg, Codec, ColorFormat, Hello, HelloReply, HostMsg, InputEvent, PROTOCOL_VERSION, WindowId, WindowInfo,
};
use umac_transport::{
    Fingerprint, Identity,
    fec::{FrameMeta, shard_frame},
    framing::{read_msg, write_msg},
    peer_fingerprint, quinn, server_endpoint,
};

use super::{
    capture::find_window,
    input::{self, Command, Target, Targets},
    permissions,
    stream::{StreamParams, WindowStream},
    windows::{self, Geometry},
    ax::AxWindow,
};
use crate::{auth::AuthStore, config, config::Settings, encode::EncodedFrame, status::Status};

const SUPPORTED: [Codec; 2] = [Codec::Hevc, Codec::H264];
const POLL: Duration = Duration::from_millis(250);
/// Encoded frames waiting for the network. Small on purpose: a backlog means stale video.
const FRAME_QUEUE: usize = 6;

struct Shared {
    settings: Settings,
    auth: AuthStore,
    targets: Targets,
    input: std_mpsc::Sender<Command>,
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
    let shared = Arc::new(Shared {
        input: input::spawn(targets.clone()),
        targets,
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
    let input_task = tokio::spawn(read_input(conn.clone(), shared.input.clone()));

    let mut session = Session { shared: shared.clone(), codec, streams: HashMap::new(), frames_tx };
    let result = session.control_loop(&mut send, recv).await;

    let windows: Vec<WindowId> = session.streams.keys().copied().collect();
    drop(session);
    if let Ok(mut t) = shared.targets.write() {
        for w in windows {
            t.remove(&w);
        }
    }
    let _ = shared.input.send(Command::Input(InputEvent::ReleaseAll));
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

async fn read_input(conn: quinn::Connection, input: std_mpsc::Sender<Command>) {
    while let Ok(mut recv) = conn.accept_uni().await {
        let input = input.clone();
        tokio::spawn(async move {
            while let Ok(Some(ev)) = read_msg::<_, InputEvent>(&mut recv).await {
                if input.send(Command::Input(ev)).is_err() {
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
}

struct Session {
    shared: Arc<Shared>,
    codec: Codec,
    streams: HashMap<WindowId, ActiveStream>,
    frames_tx: mpsc::Sender<(WindowId, EncodedFrame)>,
}

type Ctl = quinn::SendStream;

impl Session {
    async fn control_loop(&mut self, send: &mut Ctl, mut recv: quinn::RecvStream) -> anyhow::Result<()> {
        let mut poll = tokio::time::interval(POLL);
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
            }
        };
        reader.abort();
        result
    }

    async fn on_message(&mut self, msg: ClientMsg, send: &mut Ctl) -> anyhow::Result<()> {
        match msg {
            ClientMsg::Ping { nonce } => write_msg(send, &HostMsg::Pong { nonce }).await?,
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
            ClientMsg::Unsubscribe { window } => self.remove(window),
            ClientMsg::RequestKeyframe { window } => {
                if let Some(s) = self.streams.get(&window) {
                    s.stream.request_keyframe();
                }
            }
            ClientMsg::Focus { window } => {
                let _ = self.shared.input.send(Command::Focus(window));
            }
            ClientMsg::Resize { window, width, height } => {
                let Some(s) = self.streams.get(&window) else { return Ok(()) };
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
        self.streams.insert(window, ActiveStream { stream, info: info.clone(), geometry, inset });
        Ok(info)
    }

    fn set_target(&self, info: &WindowInfo, g: &Geometry, inset: f64) {
        if let Ok(mut t) = self.shared.targets.write() {
            t.insert(info.id, Target { pid: info.pid, placement: g.placement(info.scale as f64, inset) });
        }
    }

    fn remove(&mut self, window: WindowId) {
        if self.streams.remove(&window).is_some() {
            if let Ok(mut t) = self.shared.targets.write() {
                t.remove(&window);
            }
            tracing::info!(window, "stream stopped");
        }
    }

    async fn poll_windows(&mut self, send: &mut Ctl) -> anyhow::Result<()> {
        let ids: Vec<WindowId> = self.streams.keys().copied().collect();
        for id in ids {
            let Some(g) = windows::geometry(id) else {
                self.remove(id);
                write_msg(send, &HostMsg::WindowClosed { window: id }).await?;
                continue;
            };
            let s = self.streams.get_mut(&id).unwrap();
            s.stream.serve_idle_keyframe();
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
}
