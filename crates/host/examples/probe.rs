//! Connects to a running `umac-host run` and checks pairing, rejection and the control channel.
//! Usage: cargo run -p umac-host --example probe -- 'umac://<ip>:<port>?fp=<hex>&token=<token>'

use std::net::SocketAddr;

use umac_proto::{ClientMsg, Codec, Hello, HelloReply, HostMsg, PROTOCOL_VERSION};
use umac_transport::{
    Fingerprint, Identity, client_endpoint,
    framing::{read_msg, write_msg},
};

struct Pairing {
    addr: SocketAddr,
    fp: Fingerprint,
    token: String,
}

fn parse(url: &str) -> anyhow::Result<Pairing> {
    let rest = url.strip_prefix("umac://").ok_or_else(|| anyhow::anyhow!("not a umac:// url"))?;
    let (addr, query) = rest.split_once('?').ok_or_else(|| anyhow::anyhow!("missing query"))?;
    let mut fp = None;
    let mut token = None;
    for kv in query.split('&') {
        match kv.split_once('=') {
            Some(("fp", v)) => fp = Some(v.parse()?),
            Some(("token", v)) => token = Some(v.to_owned()),
            _ => {}
        }
    }
    Ok(Pairing { addr: addr.parse()?, fp: fp.ok_or_else(|| anyhow::anyhow!("no fp"))?, token: token.unwrap_or_default() })
}

async fn session(p: &Pairing, id: &Identity, token: Option<&str>, list: bool) -> anyhow::Result<HelloReply> {
    let ep = client_endpoint(id, p.fp)?;
    let conn = ep.connect(p.addr, umac_transport::SERVER_NAME)?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = Hello {
        version: PROTOCOL_VERSION,
        client_name: "probe".into(),
        codecs: vec![Codec::Hevc, Codec::H264],
        pairing_token: token.map(str::to_owned),
    };
    write_msg(&mut send, &hello).await?;
    let reply: HelloReply = read_msg(&mut recv).await?.ok_or_else(|| anyhow::anyhow!("no reply"))?;
    if matches!(reply, HelloReply::Accepted { .. }) {
        let t = std::time::Instant::now();
        write_msg(&mut send, &ClientMsg::Ping { nonce: 42 }).await?;
        let pong: Option<HostMsg> = read_msg(&mut recv).await?;
        println!("  ping -> {pong:?} in {:?}", t.elapsed());
        if list {
            write_msg(&mut send, &ClientMsg::ListWindows).await?;
            let msg: Option<HostMsg> = read_msg(&mut recv).await?;
            match msg {
                Some(HostMsg::Windows(w)) => {
                    println!("  {} windows", w.len());
                    for w in w.iter().take(10) {
                        println!("    {} {}x{} {} {:?}", w.id, w.width, w.height, w.app_name, w.title);
                    }
                }
                other => println!("  list -> {other:?}"),
            }
        }
    }
    conn.close(0u32.into(), b"bye");
    ep.wait_idle().await;
    Ok(reply)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: probe <umac:// url>"))?;
    let p = parse(&url)?;
    let me = Identity::generate()?;
    let stranger = Identity::generate()?;

    println!("1. unpaired client, no token");
    println!("  {:?}", session(&p, &stranger, None, false).await?);
    println!("2. wrong token");
    println!("  {:?}", session(&p, &stranger, Some("00"), false).await?);
    println!("3. pairing with token");
    println!("  {:?}", session(&p, &me, Some(&p.token), true).await?);
    println!("4. reconnect without token (now paired)");
    println!("  {:?}", session(&p, &me, None, false).await?);
    println!("5. token reuse by another client");
    println!("  {:?}", session(&p, &stranger, Some(&p.token), false).await?);
    let wrong = Pairing { fp: Identity::generate()?.fingerprint(), ..p };
    println!("6. wrong host fingerprint");
    println!("  {:?}", session(&wrong, &me, None, false).await.err().map(|e| e.to_string()));
    Ok(())
}
