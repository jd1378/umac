//! PyroWave check against the running agent, using the paired `umac` client identity in
//! ~/.config/umac: subscribes to the main display with PyroWave, moves the Mac pointer in a
//! circle so frames keep coming (input only, no keys or clicks), counts PyroWave datagrams per
//! frame and prints the host's encode stage stats.
//!
//!   pyro_probe [seconds]

use std::{collections::BTreeMap, time::Duration};

use umac_proto::{
    ClientMsg, Codec, Hello, HelloReply, HostMsg, InputEvent, PROTOCOL_VERSION, desktop_id,
    video::{PYRO_HEADER_LEN, PyroHeader, is_pyro},
};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
};

fn config_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/umac")
}

#[derive(Default)]
struct Frame {
    count: u16,
    seen: u16,
    bytes: usize,
    size: (u16, u16),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let seconds: u64 = std::env::args().nth(1).map_or(Ok(10), |s| s.parse())?;
    let id = Identity::load_or_create(&config_dir().join("identity"))?;
    let hosts: serde_json::Value = serde_json::from_slice(&std::fs::read(config_dir().join("hosts.json"))?)?;
    let host = &hosts[0];
    let ep = client_endpoint(&id, host["fingerprint"].as_str().unwrap().parse()?)?;
    let addr = tokio::net::lookup_host(host["addr"].as_str().unwrap()).await?.next().unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello { version: PROTOCOL_VERSION, client_name: "pyro-probe".into(), codecs: vec![Codec::PyroWave], pairing_token: None };
    write_msg(&mut send, &hello).await?;
    let reply: Option<HelloReply> = read_msg(&mut recv).await?;
    anyhow::ensure!(matches!(reply, Some(HelloReply::Accepted { .. })), "not accepted: {reply:?}");

    write_msg(&mut send, &ClientMsg::ListDisplays).await?;
    let displays = loop {
        match read_msg(&mut recv).await? {
            Some(HostMsg::Displays(d)) => break d,
            Some(_) => continue,
            None => anyhow::bail!("no display list"),
        }
    };
    let d = displays.iter().find(|d| d.main).or(displays.first()).ok_or_else(|| anyhow::anyhow!("no display"))?;
    write_msg(&mut send, &ClientMsg::WantStats { on: true }).await?;
    write_msg(&mut send, &ClientMsg::SubscribeDisplay { index: d.index, max_fps: 60, bitrate_kbps: 0 }).await?;
    let (window, w, h) = (desktop_id(d.index), d.width as f32, d.height as f32);

    tokio::spawn(async move {
        while let Ok(Some(msg)) = read_msg::<_, HostMsg>(&mut recv).await {
            match msg {
                HostMsg::StreamStarted { codec, info, .. } => println!("stream started: {codec:?} {}x{}", info.width, info.height),
                HostMsg::StreamStats { encode_p50_us, encode_p95_us, send_p50_us, frames, .. } => println!(
                    "host stats: frames {frames}, capture to encoded p50 {:.2} ms p95 {:.2} ms, send p50 {:.2} ms",
                    encode_p50_us as f64 / 1000.0,
                    encode_p95_us as f64 / 1000.0,
                    send_p50_us as f64 / 1000.0
                ),
                HostMsg::Error { message, .. } => println!("host error: {message}"),
                _ => {}
            }
        }
    });

    let mut input = conn.open_uni().await?;
    let dg = conn.clone();
    let counter = tokio::spawn(async move {
        let mut frames: BTreeMap<u32, Frame> = BTreeMap::new();
        let mut other = 0usize;
        while let Ok(d) = dg.read_datagram().await {
            if !is_pyro(&d) {
                other += 1;
                continue;
            }
            let Ok(h) = PyroHeader::parse(&d) else { continue };
            let f = frames.entry(h.frame_seq).or_default();
            f.count = h.packet_count;
            f.seen += 1;
            f.bytes += d.len() - PYRO_HEADER_LEN;
            f.size = (h.width, h.height);
        }
        (frames, other)
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut tick = tokio::time::interval(Duration::from_micros(16_667));
    for i in 0..seconds * 60 {
        tick.tick().await;
        let a = i as f32 * 0.05;
        let (x, y) = (w / 2.0 + a.cos() * w / 4.0, h / 2.0 + a.sin() * h / 4.0);
        write_msg(&mut input, &InputEvent::MouseMove { window, x, y }).await?;
    }
    tokio::time::sleep(Duration::from_millis(2500)).await;
    conn.close(0u32.into(), b"bye");
    let (frames, other) = counter.await?;
    let n = frames.len().max(1);
    let complete = frames.values().filter(|f| f.seen == f.count).count();
    let bytes: usize = frames.values().map(|f| f.bytes).sum();
    let packets: usize = frames.values().map(|f| f.seen as usize).sum();
    let size = frames.values().next().map_or((0, 0), |f| f.size);
    println!(
        "{} frames at {}x{} ({complete} complete), {:.1} packets/frame, {:.1} KB/frame, {:.1} Mbit/s over {seconds} s, non-PyroWave datagrams {other}",
        frames.len(),
        size.0,
        size.1,
        packets as f64 / n as f64,
        bytes as f64 / n as f64 / 1024.0,
        bytes as f64 * 8.0 / seconds as f64 / 1e6
    );
    Ok(())
}
