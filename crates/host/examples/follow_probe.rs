//! Checks that the Mac's focused window is followed, using the paired `umac` client
//! identity in ~/.config/umac.
//!
//!   follow_probe [quit]
//!   follow_probe close <window>   Cmd+W to a window another session streams
//!
//! 1. Opens Finder on /Applications through Launch, streams it, type-selects Calculator and
//!    opens it with Cmd+O; the host must announce Calculator's window (new or, when already
//!    running, its existing one).
//! 2. Closes that stream; the host must not reopen it while Calculator keeps focus.
//! 3. Focuses Finder, then Cmd+Tab back to Calculator; the host must announce it again.
//!
//! With `quit`, Calculator is quit with Cmd+Q afterwards. The Finder window is always closed
//! with Cmd+W.

use std::time::{Duration, Instant};

use umac_proto::{
    ClientMsg, Codec, Hello, HelloReply, HostMsg, InputEvent, LaunchAction, PROTOCOL_VERSION, WindowKind,
    WindowInfo,
    keys::{kVK_ANSI_A, kVK_ANSI_C, kVK_ANSI_L, kVK_ANSI_O, kVK_ANSI_Q, kVK_ANSI_W, kVK_Command, kVK_Tab},
};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
    quinn::{RecvStream, SendStream},
};

fn config_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/umac")
}

async fn keys(input: &mut SendStream, window: u32, chord: &[(u16, bool)]) -> anyhow::Result<()> {
    for &(keycode, pressed) in chord {
        write_msg(input, &InputEvent::Key { window, keycode, pressed }).await?;
    }
    Ok(())
}

async fn tap(input: &mut SendStream, window: u32, keycode: u16) -> anyhow::Result<()> {
    keys(input, window, &[(keycode, true), (keycode, false)]).await?;
    tokio::time::sleep(Duration::from_millis(60)).await;
    Ok(())
}

async fn cmd(input: &mut SendStream, window: u32, keycode: u16) -> anyhow::Result<()> {
    keys(input, window, &[(kVK_Command, true), (keycode, true), (keycode, false), (kVK_Command, false)]).await
}

/// Waits for an unsolicited normal window other than `except`.
async fn followed(recv: &mut RecvStream, except: u32, wait: Duration) -> anyhow::Result<Option<WindowInfo>> {
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(msg) = tokio::time::timeout_at(deadline, read_msg::<_, HostMsg>(recv)).await {
        match msg? {
            Some(HostMsg::StreamStarted { window, info, .. }) if window != except && info.kind == WindowKind::Normal => return Ok(Some(info)),
            Some(_) => continue,
            None => anyhow::bail!("connection closed"),
        }
    }
    Ok(None)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let quit = std::env::args().any(|a| a == "quit");
    let id = Identity::load_or_create(&config_dir().join("identity"))?;
    let hosts: serde_json::Value = serde_json::from_slice(&std::fs::read(config_dir().join("hosts.json"))?)?;
    let host = &hosts[0];
    let ep = client_endpoint(&id, host["fingerprint"].as_str().unwrap().parse()?)?;
    let addr = tokio::net::lookup_host(host["addr"].as_str().unwrap()).await?.next().unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello { version: PROTOCOL_VERSION, client_name: "follow-probe".into(), codecs: vec![Codec::Hevc], pairing_token: None };
    write_msg(&mut send, &hello).await?;
    let reply: Option<HelloReply> = read_msg(&mut recv).await?;
    anyhow::ensure!(matches!(reply, Some(HelloReply::Accepted { .. })), "not accepted: {reply:?}");
    let mut input = conn.open_uni().await?;

    // `close <window>`: Cmd+W to a window another session streams (test cleanup).
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "close") {
        let window: u32 = args.get(pos + 1).ok_or_else(|| anyhow::anyhow!("close <window>"))?.parse()?;
        write_msg(&mut send, &ClientMsg::Focus { window }).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        cmd(&mut input, window, kVK_ANSI_W).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        println!("sent Cmd+W to {window}");
        let _ = input.finish();
        conn.close(0u32.into(), b"bye");
        return Ok(());
    }

    write_msg(&mut send, &ClientMsg::Launch { nonce: 1, action: LaunchAction::FinderApplications }).await?;
    let finder = loop {
        match read_msg(&mut recv).await? {
            Some(HostMsg::Launched { nonce: 1, result }) => break result.map_err(anyhow::Error::msg)?,
            Some(_) => continue,
            None => anyhow::bail!("connection closed"),
        }
    };
    println!("Finder window {} \"{}\"", finder.id, finder.title);
    write_msg(&mut send, &ClientMsg::Subscribe { window: finder.id, max_fps: 30, bitrate_kbps: 4000 }).await?;
    write_msg(&mut send, &ClientMsg::Focus { window: finder.id }).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;

    for k in [kVK_ANSI_C, kVK_ANSI_A, kVK_ANSI_L, kVK_ANSI_C] {
        tap(&mut input, finder.id, k).await?;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let opened = Instant::now();
    cmd(&mut input, finder.id, kVK_ANSI_O).await?;
    println!("sent Cmd+O on the selection");

    let Some(calc) = followed(&mut recv, finder.id, Duration::from_secs(8)).await? else {
        println!("1. no window followed within 8 s");
        anyhow::bail!("not followed");
    };
    println!("1. followed window {} \"{}: {}\" {}x{} after {} ms", calc.id, calc.app_name, calc.title, calc.width, calc.height, opened.elapsed().as_millis());
    let mut datagrams = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
    while let Ok(Ok(_)) = tokio::time::timeout_at(deadline, conn.read_datagram()).await {
        datagrams += 1;
    }
    println!("   streamed {datagrams} datagrams in 1 s");

    write_msg(&mut send, &ClientMsg::Unsubscribe { window: calc.id }).await?;
    match followed(&mut recv, finder.id, Duration::from_millis(1500)).await? {
        Some(w) => println!("2. FAIL: reopened {} right after closing it", w.id),
        None => println!("2. closed stream stayed closed while Calculator kept focus"),
    }

    write_msg(&mut send, &ClientMsg::Focus { window: finder.id }).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let switched = Instant::now();
    keys(&mut input, finder.id, &[(kVK_Command, true), (kVK_Tab, true), (kVK_Tab, false), (kVK_Command, false)]).await?;
    match followed(&mut recv, finder.id, Duration::from_secs(4)).await? {
        Some(w) => println!("3. Cmd+Tab back: followed {} \"{}\" again after {} ms", w.id, w.app_name, switched.elapsed().as_millis()),
        None => println!("3. FAIL: not followed after Cmd+Tab"),
    }
    if quit {
        write_msg(&mut send, &ClientMsg::Focus { window: calc.id }).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        cmd(&mut input, calc.id, kVK_ANSI_Q).await?;
        println!("   quit Calculator with Cmd+Q");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    write_msg(&mut send, &ClientMsg::Focus { window: finder.id }).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    cmd(&mut input, finder.id, kVK_ANSI_W).await?;
    println!("closed the Finder window with Cmd+W");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = input.finish();
    conn.close(0u32.into(), b"bye");
    Ok(())
}
