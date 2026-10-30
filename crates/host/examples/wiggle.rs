//! Latency test helper, using the paired `umac` client identity in ~/.config/umac: moves the
//! Mac pointer in a circle over the main display at 60 Hz, so a desktop stream (which draws
//! the Mac cursor) produces a steady flow of frames. Input only, no keys or clicks.
//!
//!   wiggle [seconds]

use std::time::Duration;

use umac_proto::{ClientMsg, Codec, Hello, HelloReply, HostMsg, InputEvent, PROTOCOL_VERSION, desktop_id};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
};

fn config_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/umac")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let seconds: u64 = std::env::args().nth(1).map_or(Ok(15), |s| s.parse())?;
    let id = Identity::load_or_create(&config_dir().join("identity"))?;
    let hosts: serde_json::Value = serde_json::from_slice(&std::fs::read(config_dir().join("hosts.json"))?)?;
    let host = &hosts[0];
    let ep = client_endpoint(&id, host["fingerprint"].as_str().unwrap().parse()?)?;
    let addr = tokio::net::lookup_host(host["addr"].as_str().unwrap()).await?.next().unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello { version: PROTOCOL_VERSION, client_name: "wiggle".into(), codecs: vec![Codec::Hevc], pairing_token: None };
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
    // The pointer target must be a subscribed stream; keep this one cheap.
    write_msg(&mut send, &ClientMsg::SubscribeDisplay { index: d.index, max_fps: 1, bitrate_kbps: 500 }).await?;
    let (window, w, h) = (desktop_id(d.index), d.width as f32, d.height as f32);
    let mut input = conn.open_uni().await?;
    tokio::spawn(async move { while let Ok(Some(_)) = read_msg::<_, HostMsg>(&mut recv).await {} });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut tick = tokio::time::interval(Duration::from_micros(16_667));
    for i in 0..seconds * 60 {
        tick.tick().await;
        let a = i as f32 * 0.05;
        let (x, y) = (w / 2.0 + a.cos() * w / 4.0, h / 2.0 + a.sin() * h / 4.0);
        write_msg(&mut input, &InputEvent::MouseMove { window, x, y }).await?;
    }
    conn.close(0u32.into(), b"bye");
    Ok(())
}
