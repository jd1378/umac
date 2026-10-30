//! Launch test client, using the paired `umac` client identity in ~/.config/umac.
//!
//!   launch_probe terminal|activity|apps|finder ...
//!   launch_probe escape                    just press Escape (dismiss a stuck overlay)
//!
//! For each action: sends Launch, prints the answer and its latency, streams the returned
//! window for a moment and counts video datagrams, then closes the window with Cmd+W if it
//! was not open before (and quits the app with Cmd+Q if it had no windows before, except
//! Finder). A launch that fails (the Apps overlay) is dismissed with Escape.

use std::time::{Duration, Instant};

use umac_proto::{
    ClientMsg, Codec, Hello, HelloReply, HostMsg, InputEvent, LaunchAction, PROTOCOL_VERSION, WindowInfo,
    keys::{kVK_ANSI_Q, kVK_ANSI_W, kVK_Command, kVK_Escape},
};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
    quinn::{RecvStream, SendStream},
};

fn config_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/umac")
}

fn action(name: &str) -> anyhow::Result<LaunchAction> {
    Ok(match name {
        "terminal" => LaunchAction::Terminal,
        "activity" => LaunchAction::ActivityMonitor,
        "apps" => LaunchAction::Apps,
        "finder" => LaunchAction::FinderApplications,
        _ => anyhow::bail!("unknown action {name}"),
    })
}

async fn windows(send: &mut SendStream, recv: &mut RecvStream) -> anyhow::Result<Vec<WindowInfo>> {
    write_msg(send, &ClientMsg::ListWindows).await?;
    loop {
        match read_msg(recv).await? {
            Some(HostMsg::Windows(list)) => return Ok(list),
            Some(_) => continue,
            None => anyhow::bail!("connection closed"),
        }
    }
}

async fn keys(input: &mut SendStream, window: u32, chord: &[(u16, bool)]) -> anyhow::Result<()> {
    for &(keycode, pressed) in chord {
        write_msg(input, &InputEvent::Key { window, keycode, pressed }).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let escape_only = args.iter().any(|a| a == "escape");
    let actions: Vec<LaunchAction> =
        args.iter().filter(|a| *a != "escape").map(|a| action(a)).collect::<anyhow::Result<_>>()?;
    anyhow::ensure!(escape_only || !actions.is_empty(), "usage: launch_probe terminal|activity|apps|finder ... | escape");

    let id = Identity::load_or_create(&config_dir().join("identity"))?;
    let hosts: serde_json::Value = serde_json::from_slice(&std::fs::read(config_dir().join("hosts.json"))?)?;
    let host = &hosts[0];
    let ep = client_endpoint(&id, host["fingerprint"].as_str().unwrap().parse()?)?;
    let addr = tokio::net::lookup_host(host["addr"].as_str().unwrap()).await?.next().unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello { version: PROTOCOL_VERSION, client_name: "launch-probe".into(), codecs: vec![Codec::Hevc], pairing_token: None };
    write_msg(&mut send, &hello).await?;
    let reply: Option<HelloReply> = read_msg(&mut recv).await?;
    anyhow::ensure!(matches!(reply, Some(HelloReply::Accepted { .. })), "not accepted: {reply:?}");
    let mut input = conn.open_uni().await?;

    if escape_only {
        let w = windows(&mut send, &mut recv).await?;
        let target = w.first().map_or(0, |w| w.id);
        keys(&mut input, target, &[(kVK_Escape, true), (kVK_Escape, false)]).await?;
        println!("sent Escape");
    }

    for (nonce, action) in actions.into_iter().enumerate() {
        let before = windows(&mut send, &mut recv).await?;
        let started = Instant::now();
        write_msg(&mut send, &ClientMsg::Launch { nonce: nonce as u64, action }).await?;
        let result = loop {
            match read_msg(&mut recv).await? {
                Some(HostMsg::Launched { nonce: n, result }) if n == nonce as u64 => break result,
                Some(_) => continue,
                None => anyhow::bail!("connection closed"),
            }
        };
        let ms = started.elapsed().as_millis();
        let info = match result {
            Ok(info) => info,
            Err(e) => {
                println!("{action:?}: error after {ms} ms: {e}");
                if let Some(w) = before.first() {
                    keys(&mut input, w.id, &[(kVK_Escape, true), (kVK_Escape, false)]).await?;
                    println!("  sent Escape");
                }
                continue;
            }
        };
        let was_open = before.iter().any(|w| w.id == info.id);
        let app_had_windows = before.iter().any(|w| w.bundle_id == info.bundle_id);
        println!(
            "{action:?}: window {} \"{}: {}\" {}x{} after {ms} ms ({})",
            info.id,
            info.app_name,
            info.title,
            info.width,
            info.height,
            if was_open { "existing" } else { "new" }
        );

        write_msg(&mut send, &ClientMsg::Subscribe { window: info.id, max_fps: 30, bitrate_kbps: 4000 }).await?;
        let mut datagrams = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        while let Ok(Ok(_)) = tokio::time::timeout_at(deadline, conn.read_datagram()).await {
            datagrams += 1;
        }
        println!("  streamed {datagrams} datagrams in 1.5 s");
        write_msg(&mut send, &ClientMsg::Unsubscribe { window: info.id }).await?;

        if !was_open {
            write_msg(&mut send, &ClientMsg::Focus { window: info.id }).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let quit = !app_had_windows && info.bundle_id.as_deref() != Some("com.apple.finder");
            let chord = if quit { kVK_ANSI_Q } else { kVK_ANSI_W };
            keys(&mut input, info.id, &[(kVK_Command, true), (chord, true), (chord, false), (kVK_Command, false)]).await?;
            println!("  closed with Cmd+{}", if quit { "Q" } else { "W" });
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let _ = input.finish();
    tokio::time::sleep(Duration::from_millis(300)).await;
    conn.close(0u32.into(), b"bye");
    Ok(())
}
