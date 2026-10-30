//! Exercises popup streaming against a running agent: subscribes to an app window, clicks
//! in it (right click by default) to open a menu, prints the host messages and datagram
//! counts per window, then presses Escape and checks the popup closes.
//! Frames of every popup are written to `<id dir>/popup-<id>.h265` for inspection.
//! With `POPUP_CLICK=<y fraction>` it then left-clicks inside the first popup at that height.
//! With `CLOSE_TAB=1` it presses Cmd+W in the window before leaving (cleans up test tabs).
//! Usage: popups <umac:// url> <id dir> <app> [x fraction] [y fraction] [left]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use umac_proto::{
    ClientMsg, Codec, Hello, HelloReply, HostMsg, InputEvent, MouseButton, PROTOCOL_VERSION, keys,
    video::{HEADER_LEN, ShardHeader},
};
use umac_transport::fec::{Output, Reassembler};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
};

fn query(url: &str, key: &str) -> Option<String> {
    url.split_once('?')?.1.split('&').find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_owned))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, url, id_dir, app, rest @ ..] = args.as_slice() else {
        anyhow::bail!("usage: popups <url> <id dir> <app> [x] [y] [left]");
    };
    let fx: f32 = rest.first().map_or(Ok(0.5), |s| s.parse())?;
    let fy: f32 = rest.get(1).map_or(Ok(0.5), |s| s.parse())?;
    let button = if rest.get(2).is_some_and(|s| s == "left") { MouseButton::Left } else { MouseButton::Right };
    let addr = url.strip_prefix("umac://").and_then(|r| r.split('?').next()).ok_or_else(|| anyhow::anyhow!("bad url"))?;
    let fp = query(url, "fp").ok_or_else(|| anyhow::anyhow!("no fp"))?.parse()?;
    let id = Identity::load_or_create(std::path::Path::new(id_dir))?;
    let ep = client_endpoint(&id, fp)?;
    let conn = ep.connect(addr.parse()?, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello {
        version: PROTOCOL_VERSION,
        client_name: "popup-probe".into(),
        codecs: vec![Codec::Hevc],
        pairing_token: query(url, "token"),
    };
    write_msg(&mut send, &hello).await?;
    let reply: HelloReply = read_msg(&mut recv).await?.ok_or_else(|| anyhow::anyhow!("no reply"))?;
    println!("hello -> {reply:?}");
    write_msg(&mut send, &ClientMsg::ListWindows).await?;
    // Other messages (the current cursor) can arrive first.
    let list = loop {
        match read_msg(&mut recv).await? {
            Some(HostMsg::Windows(list)) => break list,
            Some(_) => continue,
            None => anyhow::bail!("no window list"),
        }
    };
    let w = list
        .iter()
        .find(|w| w.app_name.contains(app.as_str()))
        .ok_or_else(|| anyhow::anyhow!("no {app} window"))?
        .clone();
    println!("target {} {}x{} {:?}", w.id, w.width, w.height, w.title);
    write_msg(&mut send, &ClientMsg::Subscribe { window: w.id, max_fps: 60, bitrate_kbps: 0 }).await?;

    let t0 = Instant::now();
    let counts: Arc<Mutex<HashMap<u32, (usize, usize)>>> = Arc::default();
    let (c, dconn, dir, main) = (counts.clone(), conn.clone(), id_dir.clone(), w.id);
    tokio::spawn(async move {
        let mut asm: HashMap<u32, Reassembler> = HashMap::new();
        while let Ok(d) = dconn.read_datagram().await {
            if let Ok(h) = ShardHeader::parse(&d) {
                let mut m = c.lock().unwrap();
                let e = m.entry(h.window).or_default();
                e.0 += 1;
                e.1 += (h.is_keyframe() && h.shard_index == 0) as usize;
                if h.window == main {
                    continue;
                }
                for out in asm.entry(h.window).or_default().push(h, &d[HEADER_LEN..], Instant::now()) {
                    if let Output::Frame(f) = out {
                        use std::io::Write;
                        let path = format!("{dir}/popup-{}.h265", h.window);
                        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
                        file.write_all(&f.data).unwrap();
                    }
                }
            }
        }
    });
    let first_popup: Arc<Mutex<Option<(u32, u32, u32)>>> = Arc::default();
    let fp = first_popup.clone();
    tokio::spawn(async move {
        while let Ok(Some(msg)) = read_msg::<_, HostMsg>(&mut recv).await {
            match msg {
                HostMsg::StreamStarted { info, .. } if info.parent.is_some() => {
                    fp.lock().unwrap().get_or_insert((info.id, info.width, info.height));
                    println!(
                        "{:>6}ms started {} {:?} parent {:?} offset {:?} {}x{}",
                        t0.elapsed().as_millis(),
                        info.id,
                        info.kind,
                        info.parent,
                        info.offset,
                        info.width,
                        info.height
                    )
                }
                HostMsg::StreamStarted { info, .. } => println!(
                    "{:>6}ms started {} {:?} parent {:?} offset {:?} {}x{}",
                    t0.elapsed().as_millis(),
                    info.id,
                    info.kind,
                    info.parent,
                    info.offset,
                    info.width,
                    info.height
                ),
                HostMsg::WindowChanged(info) => {
                    println!("{:>6}ms changed {} offset {:?} {}x{}", t0.elapsed().as_millis(), info.id, info.offset, info.width, info.height)
                }
                other => println!("{:>6}ms {other:?}", t0.elapsed().as_millis()),
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut input = conn.open_uni().await?;
    write_msg(&mut send, &ClientMsg::Focus { window: w.id }).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (x, y) = (w.width as f32 * fx, w.height as f32 * fy);
    println!("{:>6}ms click {button:?} at {x},{y}", t0.elapsed().as_millis());
    write_msg(&mut input, &InputEvent::MouseMove { window: w.id, x, y }).await?;
    write_msg(&mut input, &InputEvent::MouseButton { window: w.id, x, y, button, pressed: true }).await?;
    write_msg(&mut input, &InputEvent::MouseButton { window: w.id, x, y, button, pressed: false }).await?;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    println!("{:>6}ms datagrams/keyframes per window: {:?}", t0.elapsed().as_millis(), counts.lock().unwrap());
    let popup = *first_popup.lock().unwrap();
    if let (Some((pid, pw, ph)), Ok(fy)) = (popup, std::env::var("POPUP_CLICK").map(|v| v.parse::<f32>())) {
        let (x, y) = (pw as f32 / 2.0, ph as f32 * fy?);
        println!("{:>6}ms click in popup {pid} at {x},{y}", t0.elapsed().as_millis());
        write_msg(&mut input, &InputEvent::MouseMove { window: pid, x, y }).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        for pressed in [true, false] {
            write_msg(&mut input, &InputEvent::MouseButton { window: pid, x, y, button: MouseButton::Left, pressed }).await?;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    println!("{:>6}ms escape", t0.elapsed().as_millis());
    for pressed in [true, false] {
        write_msg(&mut input, &InputEvent::Key { window: w.id, keycode: keys::kVK_Escape, pressed }).await?;
    }
    tokio::time::sleep(Duration::from_millis(1000)).await;
    println!("{:>6}ms datagrams/keyframes per window: {:?}", t0.elapsed().as_millis(), counts.lock().unwrap());
    if std::env::var("CLOSE_TAB").is_ok() {
        for (keycode, pressed) in [(keys::kVK_Command, true), (keys::kVK_ANSI_W, true), (keys::kVK_ANSI_W, false), (keys::kVK_Command, false)] {
            write_msg(&mut input, &InputEvent::Key { window: w.id, keycode, pressed }).await?;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    write_msg(&mut send, &ClientMsg::Unsubscribe { window: w.id }).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    conn.close(0u32.into(), b"bye");
    ep.wait_idle().await;
    Ok(())
}
