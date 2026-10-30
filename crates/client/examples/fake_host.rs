//! Minimal stand-in for the Mac host, for testing the client on one machine.
//! Serves one fake window that streams an HEVC clip in a loop and logs received input.
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
use umac_proto::{ClientMsg, Codec, ColorFormat, Hello, HelloReply, HostMsg, InputEvent, WindowInfo, WindowKind};
use umac_transport::{
    Fingerprint, Identity,
    fec::{FrameMeta, shard_frame},
    framing::{read_msg, write_msg},
    identity::random_token,
    peer_fingerprint, server_endpoint,
};

const WINDOW: u32 = 4242;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().context("usage: fake_host <state-dir> [port]")?);
    let port: u16 = args.next().map(|p| p.parse()).transpose()?.unwrap_or(umac_proto::DEFAULT_PORT);
    let identity = Identity::load_or_create(&dir.join("identity"))?;
    let clip = dir.join("clip.ts");
    if !clip.exists() {
        sample::generate_hevc(&clip, 1280, 720, 4)?;
    }
    let packets = Arc::new(sample::read_packets(&clip)?);
    let token = random_token();
    let authorized: Arc<Mutex<HashSet<Fingerprint>>> = Default::default();
    println!("umac://127.0.0.1:{port}?fp={}&token={token}", identity.fingerprint());

    let ep = server_endpoint(format!("0.0.0.0:{port}").parse()?, &identity)?;
    while let Some(incoming) = ep.accept().await {
        let (packets, token, authorized) = (packets.clone(), token.clone(), authorized.clone());
        tokio::spawn(async move {
            if let Err(e) = serve(incoming, packets, token, authorized).await {
                tracing::warn!("connection ended: {e:#}");
            }
        });
    }
    Ok(())
}

async fn serve(
    incoming: umac_transport::quinn::Incoming,
    packets: Arc<Vec<sample::SamplePacket>>,
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
    tracing::info!(client = hello.client_name, "client accepted");

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
        width: 1280,
        height: 720,
        scale: 1.0,
        parent: None,
        kind: WindowKind::Normal,
    };
    let restart = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut streaming: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(msg) = read_msg::<_, ClientMsg>(&mut recv).await? {
        tracing::info!(?msg, "control");
        match msg {
            ClientMsg::ListWindows => write_msg(&mut send, &HostMsg::Windows(vec![info.clone()])).await?,
            ClientMsg::Subscribe { window: WINDOW, .. } => {
                let msg = HostMsg::StreamStarted {
                    window: WINDOW,
                    codec: Codec::Hevc,
                    color: ColorFormat::Yuv420Bt709Limited,
                    info: info.clone(),
                };
                write_msg(&mut send, &msg).await?;
                let (conn, packets, restart) = (conn.clone(), packets.clone(), restart.clone());
                streaming = Some(tokio::spawn(async move {
                    let _ = stream(conn, packets, restart).await;
                }));
            }
            ClientMsg::Unsubscribe { .. } => {
                if let Some(s) = streaming.take() {
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
        let meta = FrameMeta { window: WINDOW, seq, keyframe: p.keyframe, capture_us: start.elapsed().as_micros() as u32 };
        let max = conn.max_datagram_size().context("datagrams disabled")?;
        for d in shard_frame(meta, &p.data, max, 10)? {
            conn.send_datagram_wait(d.into()).await?;
        }
        seq = seq.wrapping_add(1);
        i = (i + 1) % packets.len();
    }
}
