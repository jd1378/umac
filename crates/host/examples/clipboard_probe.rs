//! Clipboard test client, using the paired `umac` client identity in ~/.config/umac.
//!
//!   clipboard_probe set <text>          put text on the Mac clipboard (as a Cmd+V would)
//!   clipboard_probe watch <seconds>     tap Shift, then print Mac clipboard updates
//!   clipboard_probe esc                 press Escape (closes an open menu)
//!   clipboard_probe menu <app> [y]      right-click the app's window and print popups; with y,
//!                                       also left-click popup row y and answer a paste request

use std::time::Duration;

use umac_proto::{
    ClientMsg, Codec, Hello, HelloReply, HostMsg, InputEvent, MouseButton, PROTOCOL_VERSION,
    keys::{kVK_ANSI_U, kVK_Control, kVK_Escape, kVK_Shift},
};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
    quinn,
};

fn config_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/umac")
}

async fn connect() -> anyhow::Result<(quinn::Endpoint, quinn::Connection, quinn::SendStream, quinn::RecvStream)> {
    let id = Identity::load_or_create(&config_dir().join("identity"))?;
    let hosts: serde_json::Value = serde_json::from_slice(&std::fs::read(config_dir().join("hosts.json"))?)?;
    let host = &hosts[0];
    let ep = client_endpoint(&id, host["fingerprint"].as_str().unwrap().parse()?)?;
    let addr = tokio::net::lookup_host(host["addr"].as_str().unwrap()).await?.next().unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello { version: PROTOCOL_VERSION, client_name: "clipboard-probe".into(), codecs: vec![Codec::Hevc], pairing_token: None };
    write_msg(&mut send, &hello).await?;
    let reply: Option<HelloReply> = read_msg(&mut recv).await?;
    anyhow::ensure!(matches!(reply, Some(HelloReply::Accepted { .. })), "not accepted: {reply:?}");
    Ok((ep, conn, send, recv))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (_ep, conn, mut send, mut recv) = connect().await?;
    let mut input = conn.open_uni().await?;
    match args.first().map(String::as_str) {
        Some("set") => {
            write_msg(&mut input, &InputEvent::SetClipboard { nonce: None, text: Some(args[1].clone()) }).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Some("watch") => {
            let secs: u64 = args.get(1).map_or(Ok(3), |s| s.parse())?;
            for pressed in [true, false] {
                write_msg(&mut input, &InputEvent::Key { window: 0, keycode: kVK_Shift, pressed }).await?;
            }
            let t = std::time::Instant::now();
            let _ = tokio::time::timeout(Duration::from_secs(secs), async {
                while let Ok(Some(msg)) = read_msg::<_, HostMsg>(&mut recv).await {
                    if let HostMsg::Clipboard { text } = msg {
                        println!("clipboard after {:?}: {} bytes: {text:?}", t.elapsed(), text.len());
                    }
                }
            })
            .await;
        }
        Some("menu") => {
            write_msg(&mut send, &ClientMsg::ListWindows).await?;
            let Some(HostMsg::Windows(list)) = read_msg(&mut recv).await? else { anyhow::bail!("no list") };
            let w = list.iter().find(|w| w.app_name == args[1]).ok_or_else(|| anyhow::anyhow!("no window"))?;
            write_msg(&mut send, &ClientMsg::Subscribe { window: w.id, max_fps: 30, bitrate_kbps: 5000 }).await?;
            write_msg(&mut send, &ClientMsg::Focus { window: w.id }).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let (x, y) = (w.width as f32 / 2.0, w.height as f32 / 2.0);
            for pressed in [true, false] {
                write_msg(&mut input, &InputEvent::MouseButton { window: w.id, x, y, button: MouseButton::Right, pressed }).await?;
            }
            let row: Option<f32> = args.get(2).map(|s| s.parse()).transpose()?;
            let _ = tokio::time::timeout(Duration::from_secs(4), async {
                while let Ok(Some(msg)) = read_msg::<_, HostMsg>(&mut recv).await {
                    match msg {
                        HostMsg::StreamStarted { info, .. } if info.parent.is_some() => {
                            println!("popup {} {}x{} at {:?}", info.id, info.width, info.height, info.offset);
                            if let Some(ry) = row {
                                let px = info.width as f32 / 2.0;
                                write_msg(&mut input, &InputEvent::MouseMove { window: info.id, x: px, y: ry }).await.ok();
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                for pressed in [true, false] {
                                    let ev = InputEvent::MouseButton { window: info.id, x: px, y: ry, button: MouseButton::Left, pressed };
                                    write_msg(&mut input, &ev).await.ok();
                                }
                            }
                        }
                        HostMsg::ClipboardRequest { nonce } => {
                            println!("clipboard request {nonce}");
                            let ev = InputEvent::SetClipboard { nonce: Some(nonce), text: Some("umac-menu-paste".into()) };
                            write_msg(&mut input, &ev).await.ok();
                        }
                        HostMsg::WindowClosed { window } => println!("closed {window}"),
                        _ => {}
                    }
                }
            })
            .await;
            write_msg(&mut send, &ClientMsg::Unsubscribe { window: w.id }).await?;
        }
        Some("clearline") => {
            for (k, p) in [(kVK_Control, true), (kVK_ANSI_U, true), (kVK_ANSI_U, false), (kVK_Control, false)] {
                write_msg(&mut input, &InputEvent::Key { window: 0, keycode: k, pressed: p }).await?;
            }
        }
        Some("esc") => {
            for pressed in [true, false] {
                write_msg(&mut input, &InputEvent::Key { window: 0, keycode: kVK_Escape, pressed }).await?;
            }
        }
        _ => anyhow::bail!("usage: clipboard_probe set <text> | watch <secs> | menu <app> [row_y]"),
    }
    let _ = input.finish();
    tokio::time::sleep(Duration::from_millis(200)).await;
    conn.close(0u32.into(), b"bye");
    Ok(())
}
