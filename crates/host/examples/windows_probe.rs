//! Lists the host's windows with their resizability, using the paired client identity in
//! ~/.config/umac. Also checks that an old protocol peer gets a readable rejection.
//! Usage: cargo run -p umac-host --example windows_probe [-- old | close <id>...]

use umac_proto::{ClientMsg, Codec, Hello, HelloReply, HostMsg, PROTOCOL_VERSION};
use umac_transport::{
    Identity, client_endpoint,
    framing::{read_msg, write_msg},
};

fn config_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".config/umac")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let old = std::env::args().any(|a| a == "old");
    let id = Identity::load_or_create(&config_dir().join("identity"))?;
    let hosts: serde_json::Value = serde_json::from_slice(&std::fs::read(config_dir().join("hosts.json"))?)?;
    let host = &hosts[0];
    let ep = client_endpoint(&id, host["fingerprint"].as_str().unwrap().parse()?)?;
    let addr = tokio::net::lookup_host(host["addr"].as_str().unwrap()).await?.next().unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME)?.await?;
    println!("negotiated protocol {:?}", umac_transport::negotiated_version(&conn));
    let (mut send, mut recv) = conn.open_bi().await?;
    // `old` pretends to be a protocol 2 client to see the host's rejection text.
    let version = if old { PROTOCOL_VERSION - 1 } else { PROTOCOL_VERSION };
    let hello = Hello { version, client_name: "windows-probe".into(), codecs: vec![Codec::PyroWave], pairing_token: None };
    write_msg(&mut send, &hello).await?;
    let reply: Option<HelloReply> = read_msg(&mut recv).await?;
    let Some(HelloReply::Accepted { .. }) = reply else {
        println!("reply: {reply:?}");
        return Ok(());
    };
    // `close <id>...`: subscribe each window, then press its close button like the client does.
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "close") {
        for w in &args[pos + 1..] {
            let window: u32 = w.parse()?;
            write_msg(&mut send, &ClientMsg::Subscribe { window, max_fps: 5, bitrate_kbps: 1000 }).await?;
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            write_msg(&mut send, &ClientMsg::CloseWindow { window }).await?;
            println!("closing {window}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        conn.close(0u32.into(), b"bye");
        return Ok(());
    }
    write_msg(&mut send, &ClientMsg::ListWindows).await?;
    loop {
        match read_msg::<_, HostMsg>(&mut recv).await? {
            Some(HostMsg::Windows(list)) => {
                for w in list {
                    println!("{:>6} {:<20} {:>5}x{:<5} resizable={} {:?}", w.id, w.app_name, w.width, w.height, w.resizable, w.title);
                }
                break;
            }
            Some(_) => continue,
            None => anyhow::bail!("connection closed"),
        }
    }
    conn.close(0u32.into(), b"bye");
    ep.wait_idle().await;
    Ok(())
}
