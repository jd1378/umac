//! Minimal stand-in for the Mac host, for testing the client on one machine.
//! Serves one fake window that streams an HEVC clip in a loop and logs received input.
//! After subscribing it also opens two popups (one inside the window, one hanging past its
//! top-right corner) and closes them a few seconds later.
//!
//!   cargo run -p umac-client --example fake_host -- <state-dir> [port]

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use umac_client::sample;
use umac_proto::{ClientMsg, Codec, ColorFormat, Hello, HelloReply, HostMsg, InputEvent, MacSession, WindowInfo, WindowKind};
use umac_transport::{
    Fingerprint, Identity,
    fec::{FrameMeta, shard_frame},
    framing::{read_msg, write_msg},
    identity::random_token,
    peer_fingerprint, server_endpoint,
};

const WINDOW: u32 = 4242;
const POPUP_IN: u32 = 4243;
const POPUP_OUT: u32 = 4244;
const POPUP_SIZE: (u32, u32) = (320, 192);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().context("usage: fake_host <state-dir> [port]")?);
    let port: u16 = args.next().map(|p| p.parse()).transpose()?.unwrap_or(umac_proto::DEFAULT_PORT);
    let identity = Identity::load_or_create(&dir.join("identity"))?;
    let packets = match mjpeg_files() {
        Some(files) => Arc::new(files),
        None => {
            let (clip_w, clip_h) = clip_size();
            let clip = dir.join(format!("clip-{clip_w}x{clip_h}.ts"));
            if !clip.exists() {
                sample::generate_hevc(&clip, clip_w, clip_h, 4)?;
            }
            Arc::new(sample::read_packets(&clip)?)
        }
    };
    let popup_clip = dir.join("popup.ts");
    if !popup_clip.exists() {
        sample::generate_hevc(&popup_clip, POPUP_SIZE.0, POPUP_SIZE.1, 4)?;
    }
    let popup_packets = Arc::new(sample::read_packets(&popup_clip)?);
    let token = random_token();
    let authorized: Arc<Mutex<HashSet<Fingerprint>>> = Default::default();
    println!("umac://127.0.0.1:{port}?fp={}&token={token}", identity.fingerprint());

    let ep = server_endpoint(format!("0.0.0.0:{port}").parse()?, &identity)?;
    while let Some(incoming) = ep.accept().await {
        let (packets, popup_packets, token, authorized) =
            (packets.clone(), popup_packets.clone(), token.clone(), authorized.clone());
        tokio::spawn(async move {
            if let Err(e) = serve(incoming, packets, popup_packets, token, authorized).await {
                tracing::warn!("connection ended: {e:#}");
            }
        });
    }
    Ok(())
}

/// UMAC_FAKE_MJPEG=a.jpg,b.jpg streams those 1920x1080 JPEGs in a loop as Motion-JPEG.
fn mjpeg_files() -> Option<Vec<sample::SamplePacket>> {
    let list = std::env::var("UMAC_FAKE_MJPEG").ok()?;
    Some(list.split(',').map(|f| sample::SamplePacket { data: std::fs::read(f).expect("reading JPEG"), keyframe: true }).collect())
}

/// UMAC_FAKE_SIZE=1920x1080 streams a larger clip, e.g. for decode benchmarks.
fn clip_size() -> (u32, u32) {
    if std::env::var_os("UMAC_FAKE_MJPEG").is_some() {
        return (1920, 1080);
    }
    std::env::var("UMAC_FAKE_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1280, 720))
}

async fn serve(
    incoming: umac_transport::quinn::Incoming,
    packets: Arc<Vec<sample::SamplePacket>>,
    popup_packets: Arc<Vec<sample::SamplePacket>>,
    token: String,
    authorized: Arc<Mutex<HashSet<Fingerprint>>>,
) -> Result<()> {
    let conn = incoming.await?;
    let fp = peer_fingerprint(&conn).context("no client cert")?;
    let (mut send, mut recv) = conn.accept_bi().await?;
    let hello: Hello = read_msg(&mut recv).await?.context("no hello")?;
    let known = authorized.lock().unwrap().contains(&fp);
    if !known && hello.pairing_token.as_deref() != Some(token.as_str()) {
        write_msg(&mut send, &HelloReply::Rejected { reason: "not paired".into() }).await?;
        return Ok(());
    }
    authorized.lock().unwrap().insert(fp);
    write_msg(&mut send, &HelloReply::Accepted { host_name: "fake-host".into() }).await?;
    let mac = MacSession { user: "fake".into(), machine: "fake-machine".into(), on_screen: true };
    write_msg(&mut send, &HostMsg::Session(mac)).await?;
    tracing::info!(client = hello.client_name, "client accepted");

    let (clip_w, clip_h) = clip_size();
    let input_conn = conn.clone();
    tokio::spawn(async move {
        let Ok(mut uni) = input_conn.accept_uni().await else { return };
        while let Ok(Some(ev)) = read_msg::<_, InputEvent>(&mut uni).await {
            tracing::info!(?ev, "input");
        }
    });

    let info = WindowInfo {
        id: WINDOW,
        pid: 1,
        app_name: "FakeApp".into(),
        bundle_id: Some("dev.umac.fake".into()),
        title: "test pattern".into(),
        width: clip_w,
        height: clip_h,
        scale: 1.0,
        parent: None,
        offset: (0, 0),
        kind: WindowKind::Normal,
        resizable: true,
    };
    let popups = [(POPUP_IN, (100, 80)), (POPUP_OUT, (clip_w as i32 - POPUP_SIZE.0 as i32 / 2, -60))].map(|(id, offset)| WindowInfo {
        id,
        title: String::new(),
        width: POPUP_SIZE.0,
        height: POPUP_SIZE.1,
        parent: Some(WINDOW),
        offset,
        kind: WindowKind::Popup,
        ..info.clone()
    });
    let restart = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut streaming: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let (popup_tx, mut popup_rx) = tokio::sync::mpsc::channel::<HostMsg>(8);
    loop {
        let msg = tokio::select! {
            m = read_msg::<_, ClientMsg>(&mut recv) => match m? { Some(m) => m, None => break },
            Some(out) = popup_rx.recv() => {
                write_msg(&mut send, &out).await?;
                continue;
            }
        };
        tracing::info!(?msg, "control");
        match msg {
            ClientMsg::ListWindows => write_msg(&mut send, &HostMsg::Windows(vec![info.clone()])).await?,
            ClientMsg::Subscribe { window: WINDOW, .. } => {
                let codec = if std::env::var_os("UMAC_FAKE_MJPEG").is_some() { Codec::Mjpeg } else { Codec::Hevc };
                let msg = HostMsg::StreamStarted {
                    window: WINDOW,
                    codec,
                    color: ColorFormat::Yuv420Bt709Limited,
                    info: info.clone(),
                };
                write_msg(&mut send, &msg).await?;
                let (c, p, r) = (conn.clone(), packets.clone(), restart.clone());
                streaming.push(tokio::spawn(async move {
                    let _ = stream(c, WINDOW, p, r).await;
                }));
                let (conn, popup_packets, restart, popups, tx) =
                    (conn.clone(), popup_packets.clone(), restart.clone(), popups.clone(), popup_tx.clone());
                streaming.push(tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let mut tasks = Vec::new();
                    for p in popups {
                        let start = HostMsg::StreamStarted {
                            window: p.id,
                            codec: Codec::Hevc,
                            color: ColorFormat::Yuv420Bt709Limited,
                            info: p.clone(),
                        };
                        if tx.send(start).await.is_err() {
                            return;
                        }
                        let (c, pk, r) = (conn.clone(), popup_packets.clone(), restart.clone());
                        tasks.push(tokio::spawn(async move {
                            let _ = stream(c, p.id, pk, r).await;
                        }));
                    }
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    for (t, id) in tasks.into_iter().zip([POPUP_IN, POPUP_OUT]) {
                        t.abort();
                        let _ = tx.send(HostMsg::WindowClosed { window: id }).await;
                    }
                }));
            }
            ClientMsg::Unsubscribe { window: WINDOW } => {
                for s in streaming.drain(..) {
                    s.abort();
                }
            }
            ClientMsg::RequestKeyframe { .. } => restart.store(true, std::sync::atomic::Ordering::Relaxed),
            ClientMsg::Ping { nonce } => write_msg(&mut send, &HostMsg::Pong { nonce }).await?,
            _ => {}
        }
    }
    Ok(())
}

async fn stream(
    conn: umac_transport::quinn::Connection,
    window: u32,
    packets: Arc<Vec<sample::SamplePacket>>,
    restart: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_micros(16_667));
    let mut seq = 0u32;
    let mut i = 0usize;
    loop {
        tick.tick().await;
        if restart.swap(false, std::sync::atomic::Ordering::Relaxed) {
            i = 0;
        }
        let p = &packets[i % packets.len()];
        // Looping restarts at a keyframe only at index 0; that is where the clip starts.
        let meta = FrameMeta { window, seq, keyframe: p.keyframe, capture_us: start.elapsed().as_micros() as u32 };
        let max = conn.max_datagram_size().context("datagrams disabled")?;
        for d in shard_frame(meta, &p.data, max, 10)? {
            conn.send_datagram_wait(d.into()).await?;
        }
        seq = seq.wrapping_add(1);
        i = (i + 1) % packets.len();
    }
}
