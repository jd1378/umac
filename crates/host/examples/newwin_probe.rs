//! Followed-window test client, using the paired `umac` client identity in ~/.config/umac.
//!
//!   newwin_probe [app name, default TextEdit]
//!   newwin_probe cleanup        save and close the test document, quit TextEdit
//!
//! Streams the app's first window, then opens a new window (Cmd+N) and a Save sheet on it
//! (Cmd+S), the Open panel (Cmd+O) and Settings (Cmd+,), printing the windows the host starts following
//! and closes, and dismisses each one again (Cmd+W or Escape). Nothing is saved.

use std::time::{Duration, Instant};

use umac_proto::{
    ClientMsg, Codec, Hello, HelloReply, HostMsg, InputEvent, PROTOCOL_VERSION, WindowId, WindowInfo,
    keys::*,
};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
    quinn::{RecvStream, SendStream},
};

fn config_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/umac")
}

async fn chord(input: &mut SendStream, window: WindowId, key: u16, command: bool) -> anyhow::Result<()> {
    let mut seq = vec![(key, true), (key, false)];
    if command {
        seq.insert(0, (kVK_Command, true));
        seq.push((kVK_Command, false));
    }
    for (keycode, pressed) in seq {
        write_msg(input, &InputEvent::Key { window, keycode, pressed }).await?;
    }
    Ok(())
}

/// Prints host messages about windows for `wait`; returns windows that started and are open.
async fn watch(recv: &mut RecvStream, wait: Duration, open: &mut Vec<WindowInfo>, t0: Instant) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let msg = match tokio::time::timeout_at(deadline, read_msg::<_, HostMsg>(recv)).await {
            Err(_) => return Ok(()),
            Ok(m) => m?.ok_or_else(|| anyhow::anyhow!("connection closed"))?,
        };
        let ms = t0.elapsed().as_millis();
        match msg {
            HostMsg::StreamStarted { info, .. } => {
                println!(
                    "  +{ms} ms started {} {:?} parent={:?} offset={:?} {}x{} \"{}: {}\"",
                    info.id, info.kind, info.parent, info.offset, info.width, info.height, info.app_name, info.title
                );
                open.push(info);
            }
            HostMsg::WindowChanged(info) => println!("  +{ms} ms changed {} offset={:?} {}x{}", info.id, info.offset, info.width, info.height),
            HostMsg::WindowClosed { window } => {
                println!("  +{ms} ms closed {window}");
                open.retain(|w| w.id != window);
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = std::env::args().nth(1).unwrap_or_else(|| "TextEdit".into());
    let id = Identity::load_or_create(&config_dir().join("identity"))?;
    let hosts: serde_json::Value = serde_json::from_slice(&std::fs::read(config_dir().join("hosts.json"))?)?;
    let host = &hosts[0];
    let ep = client_endpoint(&id, host["fingerprint"].as_str().unwrap().parse()?)?;
    let addr = tokio::net::lookup_host(host["addr"].as_str().unwrap()).await?.next().unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello { version: PROTOCOL_VERSION, client_name: "newwin-probe".into(), codecs: vec![Codec::Hevc], pairing_token: None };
    write_msg(&mut send, &hello).await?;
    let reply: Option<HelloReply> = read_msg(&mut recv).await?;
    anyhow::ensure!(matches!(reply, Some(HelloReply::Accepted { .. })), "not accepted: {reply:?}");
    let mut input = conn.open_uni().await?;
    // Drain datagrams so the connection does not back up.
    let dg = conn.clone();
    tokio::spawn(async move { while dg.read_datagram().await.is_ok() {} });

    write_msg(&mut send, &ClientMsg::ListWindows).await?;
    let list = loop {
        match read_msg(&mut recv).await? {
            Some(HostMsg::Windows(list)) => break list,
            Some(_) => continue,
            None => anyhow::bail!("connection closed"),
        }
    };
    // Single steps, for careful cleanup: `focus <id>` or `key <id> esc|cmd-s|cmd-w|cmd-q|cmd-n|cmd-o|cmd-comma`.
    let rest: Vec<String> = std::env::args().skip(2).collect();
    // `type <id> <text>`: lower case letters, space, return, `-`, `/` and `.`, one key every
    // 120 ms (typing test).
    if app == "type" {
        let window: WindowId = rest.first().ok_or_else(|| anyhow::anyhow!("window id missing"))?.parse()?;
        write_msg(&mut send, &ClientMsg::Focus { window }).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let letters = [
            kVK_ANSI_A, kVK_ANSI_B, kVK_ANSI_C, kVK_ANSI_D, kVK_ANSI_E, kVK_ANSI_F, kVK_ANSI_G, kVK_ANSI_H, kVK_ANSI_I, kVK_ANSI_J, kVK_ANSI_K, kVK_ANSI_L, kVK_ANSI_M,
            kVK_ANSI_N, kVK_ANSI_O, kVK_ANSI_P, kVK_ANSI_Q, kVK_ANSI_R, kVK_ANSI_S, kVK_ANSI_T, kVK_ANSI_U, kVK_ANSI_V, kVK_ANSI_W, kVK_ANSI_X, kVK_ANSI_Y, kVK_ANSI_Z,
        ];
        for c in rest.get(1).map(String::as_str).unwrap_or("").chars() {
            let key = match c {
                'a'..='z' => letters[(c as u8 - b'a') as usize],
                ' ' => kVK_Space,
                '\n' => kVK_Return,
                '-' => kVK_ANSI_Minus,
                '/' => kVK_ANSI_Slash,
                '.' => kVK_ANSI_Period,
                _ => continue,
            };
            chord(&mut input, window, key, false).await?;
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let _ = input.finish();
        tokio::time::sleep(Duration::from_millis(400)).await;
        return Ok(());
    }
    // `close <id>`: press the window's close button through Accessibility (no keyboard).
    if app == "close" {
        let window: WindowId = rest.first().ok_or_else(|| anyhow::anyhow!("window id missing"))?.parse()?;
        // The host only closes windows this session streams.
        write_msg(&mut send, &ClientMsg::Subscribe { window, max_fps: 1, bitrate_kbps: 1000 }).await?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        write_msg(&mut send, &ClientMsg::CloseWindow { window }).await?;
        tokio::time::sleep(Duration::from_millis(600)).await;
        println!("close {window} sent");
        return Ok(());
    }
    if app == "focus" || app == "key" {
        let window: WindowId = rest.first().ok_or_else(|| anyhow::anyhow!("window id missing"))?.parse()?;
        if app == "focus" {
            write_msg(&mut send, &ClientMsg::Focus { window }).await?;
        } else {
            let (key, command) = match rest.get(1).map(String::as_str) {
                Some("esc") => (kVK_Escape, false),
                Some("cmd-s") => (kVK_ANSI_S, true),
                Some("cmd-w") => (kVK_ANSI_W, true),
                Some("cmd-q") => (kVK_ANSI_Q, true),
                Some("cmd-n") => (kVK_ANSI_N, true),
                Some("cmd-o") => (kVK_ANSI_O, true),
                Some("cmd-comma") => (kVK_ANSI_Comma, true),
                k => anyhow::bail!("unknown key {k:?}"),
            };
            chord(&mut input, window, key, command).await?;
        }
        let _ = input.finish();
        tokio::time::sleep(Duration::from_millis(600)).await;
        println!("{app} {rest:?} sent");
        return Ok(());
    }
    if app == "cleanup" {
        // Dismiss leftovers, save and close the test document, quit TextEdit.
        let doc = list.iter().find(|w| w.title == "umac-newwin.txt").ok_or_else(|| anyhow::anyhow!("no test document"))?;
        chord(&mut input, doc.id, kVK_Escape, false).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        write_msg(&mut send, &ClientMsg::Focus { window: doc.id }).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        for key in [kVK_ANSI_S, kVK_ANSI_W, kVK_ANSI_Q] {
            chord(&mut input, doc.id, key, true).await?;
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        println!("cleaned up {}", doc.id);
        let _ = input.finish();
        tokio::time::sleep(Duration::from_millis(300)).await;
        return Ok(());
    }
    // The app's largest window, so a leftover popover or alert is not picked.
    let main = list
        .iter()
        .filter(|w| w.app_name == app)
        .max_by_key(|w| w.width * w.height)
        .ok_or_else(|| anyhow::anyhow!("no {app} window"))?
        .clone();
    println!("streaming {} \"{}\"", main.id, main.title);
    write_msg(&mut send, &ClientMsg::Subscribe { window: main.id, max_fps: 30, bitrate_kbps: 4000 }).await?;
    write_msg(&mut send, &ClientMsg::Focus { window: main.id }).await?;
    let t0 = Instant::now();
    let mut open = Vec::new();
    watch(&mut recv, Duration::from_millis(800), &mut open, t0).await?;
    open.clear();

    // Cmd+N, then a Save sheet on the untitled document (Escape), then close it.
    println!("Cmd+N");
    chord(&mut input, main.id, kVK_ANSI_N, true).await?;
    watch(&mut recv, Duration::from_millis(2000), &mut open, t0).await?;
    if let Some(doc) = open.iter().find(|w| w.kind == umac_proto::WindowKind::Normal).cloned() {
        println!("Cmd+S on {}", doc.id);
        write_msg(&mut send, &ClientMsg::Focus { window: doc.id }).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        chord(&mut input, doc.id, kVK_ANSI_S, true).await?;
        watch(&mut recv, Duration::from_millis(2500), &mut open, t0).await?;
        chord(&mut input, doc.id, kVK_Escape, false).await?;
        watch(&mut recv, Duration::from_millis(1500), &mut open, t0).await?;
        println!("Cmd+W on {}", doc.id);
        chord(&mut input, doc.id, kVK_ANSI_W, true).await?;
        watch(&mut recv, Duration::from_millis(1500), &mut open, t0).await?;
    }
    for (name, key, escape) in [("Cmd+O", kVK_ANSI_O, true), ("Cmd+,", kVK_ANSI_Comma, false)] {
        println!("{name}");
        write_msg(&mut send, &ClientMsg::Focus { window: main.id }).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        chord(&mut input, main.id, key, true).await?;
        watch(&mut recv, Duration::from_millis(2500), &mut open, t0).await?;
        for w in open.clone() {
            if escape {
                chord(&mut input, w.id, kVK_Escape, false).await?;
            } else {
                write_msg(&mut send, &ClientMsg::Focus { window: w.id }).await?;
                tokio::time::sleep(Duration::from_millis(300)).await;
                chord(&mut input, w.id, kVK_ANSI_W, true).await?;
            }
        }
        watch(&mut recv, Duration::from_millis(1500), &mut open, t0).await?;
    }
    if !open.is_empty() {
        println!("still open: {:?}", open.iter().map(|w| w.id).collect::<Vec<_>>());
    }
    let _ = input.finish();
    tokio::time::sleep(Duration::from_millis(300)).await;
    conn.close(0u32.into(), b"bye");
    Ok(())
}
