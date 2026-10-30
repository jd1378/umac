use std::time::Instant;

use umac_proto::{ClientMsg, video::{HEADER_LEN, ShardHeader}};
use umac_transport::{
    Identity, client_endpoint,
    fec::{FrameMeta, Output, Reassembler, shard_frame},
    framing::{read_msg, write_msg},
    peer_fingerprint, server_endpoint,
};

#[tokio::test]
async fn pinned_handshake_stream_and_datagrams() {
    let host = Identity::generate().unwrap();
    let client = Identity::generate().unwrap();
    let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &host).unwrap();
    let addr = server.local_addr().unwrap();
    let client_fp = client.fingerprint();

    let frame: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
    let expected = frame.clone();
    let srv = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        assert_eq!(peer_fingerprint(&conn), Some(client_fp));
        let (_send, mut recv) = conn.accept_bi().await.unwrap();
        let msg: ClientMsg = read_msg(&mut recv).await.unwrap().unwrap();
        assert_eq!(msg, ClientMsg::Ping { nonce: 1 });
        let max = conn.max_datagram_size().unwrap();
        let meta = FrameMeta { window: 3, seq: 0, keyframe: true, capture_us: 0 };
        for d in shard_frame(meta, &frame, max, 10).unwrap() {
            conn.send_datagram_wait(d.into()).await.unwrap();
        }
        conn.closed().await;
    });

    let ep = client_endpoint(&client, host.fingerprint()).unwrap();
    let conn = ep.connect(addr, umac_transport::SERVER_NAME).unwrap().await.unwrap();
    assert_eq!(umac_transport::negotiated_version(&conn), Some(umac_proto::PROTOCOL_VERSION));
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    write_msg(&mut send, &ClientMsg::Ping { nonce: 1 }).await.unwrap();

    let mut r = Reassembler::default();
    let data = loop {
        let d = conn.read_datagram().await.unwrap();
        let h = ShardHeader::parse(&d).unwrap();
        if let Some(Output::Frame(f)) = r.push(h, &d[HEADER_LEN..], Instant::now()).pop() {
            break f.data;
        }
    };
    assert_eq!(data, expected);
    conn.close(0u32.into(), b"done");
    srv.await.unwrap();
}

#[tokio::test]
async fn rejects_wrong_host_fingerprint() {
    let host = Identity::generate().unwrap();
    let impostor = Identity::generate().unwrap();
    let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &impostor).unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        if let Some(inc) = server.accept().await {
            let _ = inc.await;
        }
    });
    let ep = client_endpoint(&Identity::generate().unwrap(), host.fingerprint()).unwrap();
    assert!(ep.connect(addr, umac_transport::SERVER_NAME).unwrap().await.is_err());
}
