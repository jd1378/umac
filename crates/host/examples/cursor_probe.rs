//! Cursor sync test client, using the paired `umac` client identity in ~/.config/umac.
//!
//!   cursor_probe <app> [cols rows]   subscribe to the app's window, sweep the pointer over a
//!                                    grid of points and print each cursor change
//!   cursor_probe <app> close         focus the app's window and press Cmd+W

use std::time::Duration;

use umac_proto::{
    ClientMsg, Codec, CursorShape, Hello, HelloReply, HostMsg, InputEvent, PROTOCOL_VERSION,
    keys::{kVK_ANSI_W, kVK_Command},
};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
};

fn config_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/umac")
}

fn describe(msg: &HostMsg) -> Option<String> {
    match msg {
        HostMsg::Cursor(c) => Some(match &c.shape {
            CursorShape::Named(n) => format!("{n:?}"),
            CursorShape::Image { width, height, hot_x, hot_y, .. } => format!("image {width}x{height} hot ({hot_x},{hot_y}) id {:x}", c.id),
        }),
        HostMsg::CursorKnown { id } => Some(format!("cached image {id:x}")),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let app = args.first().ok_or_else(|| anyhow::anyhow!("usage: cursor_probe <app> [cols rows]"))?;
    let close = args.get(1).is_some_and(|a| a == "close");
    let cols: u32 = if close { 1 } else { args.get(1).map_or(Ok(8), |s| s.parse())? };
    let rows: u32 = args.get(2).map_or(Ok(6), |s| s.parse())?;

    let id = Identity::load_or_create(&config_dir().join("identity"))?;
    let hosts: serde_json::Value = serde_json::from_slice(&std::fs::read(config_dir().join("hosts.json"))?)?;
    let host = &hosts[0];
    let ep = client_endpoint(&id, host["fingerprint"].as_str().unwrap().parse()?)?;
    let addr = tokio::net::lookup_host(host["addr"].as_str().unwrap()).await?.next().unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello { version: PROTOCOL_VERSION, client_name: "cursor-probe".into(), codecs: vec![Codec::Hevc], pairing_token: None };
    write_msg(&mut send, &hello).await?;
    let reply: Option<HelloReply> = read_msg(&mut recv).await?;
    anyhow::ensure!(matches!(reply, Some(HelloReply::Accepted { .. })), "not accepted: {reply:?}");
    let mut input = conn.open_uni().await?;

    write_msg(&mut send, &ClientMsg::ListWindows).await?;
    // Other messages (the current cursor) can arrive first.
    let list = loop {
        match read_msg(&mut recv).await? {
            Some(HostMsg::Windows(list)) => break list,
            Some(_) => continue,
            None => anyhow::bail!("no window list"),
        }
    };
    let w = list.iter().find(|w| &w.app_name == app).ok_or_else(|| anyhow::anyhow!("no {app} window"))?;
    write_msg(&mut send, &ClientMsg::Subscribe { window: w.id, max_fps: 10, bitrate_kbps: 2000 }).await?;
    write_msg(&mut send, &ClientMsg::Focus { window: w.id }).await?;
    let (width, height, window) = (w.width as f32, w.height as f32, w.id);
    if close {
        tokio::time::sleep(Duration::from_millis(400)).await;
        for (keycode, pressed) in [(kVK_Command, true), (kVK_ANSI_W, true), (kVK_ANSI_W, false), (kVK_Command, false)] {
            write_msg(&mut input, &InputEvent::Key { window, keycode, pressed }).await?;
        }
        let _ = input.finish();
        tokio::time::sleep(Duration::from_millis(500)).await;
        conn.close(0u32.into(), b"bye");
        return Ok(());
    }

    let sweep = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        for r in 0..rows {
            for c in 0..cols {
                let (x, y) = ((c as f32 + 0.5) * width / cols as f32, (r as f32 + 0.5) * height / rows as f32);
                println!("move ({x:.0}, {y:.0})");
                write_msg(&mut input, &InputEvent::MouseMove { window, x, y }).await?;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        anyhow::Ok(())
    };
    let listen = async {
        while let Ok(Some(msg)) = read_msg::<_, HostMsg>(&mut recv).await {
            if let Some(d) = describe(&msg) {
                println!("  cursor: {d}");
            }
        }
    };
    tokio::select! {
        r = sweep => r?,
        _ = listen => {}
    }
    write_msg(&mut send, &ClientMsg::Unsubscribe { window }).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    conn.close(0u32.into(), b"bye");
    Ok(())
}
