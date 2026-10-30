//! QUIC transport shared by host and client: identities, certificate pinning, stream framing
//! and FEC-protected video datagrams.

pub mod fec;
pub mod framing;
pub mod identity;
pub mod tls;

use std::{net::SocketAddr, sync::Arc, time::Duration};

pub use identity::{Fingerprint, Identity};
pub use quinn;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("invalid fingerprint")]
    BadFingerprint,
    #[error("quic config: {0}")]
    Config(String),
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    // Video frames are bursty; a keyframe can be several hundred KB. Big buffers keep the
    // congestion window and datagram queues from becoming the bottleneck on a LAN.
    t.datagram_receive_buffer_size(Some(16 << 20));
    t.datagram_send_buffer_size(16 << 20);
    t.send_window(16 << 20);
    t.initial_mtu(1400);
    let mut cc = quinn::congestion::CubicConfig::default();
    cc.initial_window(2 << 20);
    t.congestion_controller_factory(Arc::new(cc));
    t
}

/// Server endpoint for the Mac host. Any client certificate is accepted at the TLS layer;
/// the host authorizes the peer fingerprint right after the handshake.
pub fn server_endpoint(addr: SocketAddr, identity: &Identity) -> Result<quinn::Endpoint, Error> {
    let tls = tls::server_config(identity)?;
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|e| Error::Config(e.to_string()))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config()));
    endpoint(addr, Some(cfg))
}

/// Client endpoint that only accepts a host presenting the pinned certificate.
pub fn client_endpoint(identity: &Identity, host_fingerprint: Fingerprint) -> Result<quinn::Endpoint, Error> {
    let tls = tls::client_config(identity, host_fingerprint)?;
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| Error::Config(e.to_string()))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config()));
    let mut ep = endpoint("[::]:0".parse().unwrap(), None)
        .or_else(|_| endpoint("0.0.0.0:0".parse().unwrap(), None))?;
    ep.set_default_client_config(cfg);
    Ok(ep)
}

/// Kernel UDP buffer we ask for. A keyframe arrives as one burst of a few hundred datagrams,
/// which overflows default buffers (about 208 KB on Linux) and loses most of the frame.
/// The OS clamps this to its limit (`net.core.rmem_max` on Linux, `kern.ipc.maxsockbuf` on macOS).
const SOCKET_BUFFER: usize = 8 << 20;

fn endpoint(addr: SocketAddr, server: Option<quinn::ServerConfig>) -> Result<quinn::Endpoint, Error> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(false)?;
    }
    for size in [SOCKET_BUFFER, SOCKET_BUFFER / 2, SOCKET_BUFFER / 4] {
        if socket.set_recv_buffer_size(size).is_ok() {
            break;
        }
    }
    let _ = socket.set_send_buffer_size(SOCKET_BUFFER / 2);
    let got = socket.recv_buffer_size().unwrap_or(0);
    if got < SOCKET_BUFFER / 4 {
        tracing::warn!(got, "UDP receive buffer is small; raise net.core.rmem_max for fewer lost frames");
    }
    socket.bind(&addr.into())?;
    let runtime = quinn::default_runtime().ok_or_else(|| Error::Config("no async runtime".into()))?;
    Ok(quinn::Endpoint::new(quinn::EndpointConfig::default(), server, socket.into(), runtime)?)
}

/// Protocol version the handshake settled on (`umac/N`); a legacy one is only accepted so the
/// peer can be told to update.
pub fn negotiated_version(conn: &quinn::Connection) -> Option<u32> {
    let alpn = conn.handshake_data()?.downcast::<quinn::crypto::rustls::HandshakeData>().ok()?.protocol?;
    std::str::from_utf8(alpn.strip_prefix(b"umac/")?).ok()?.parse().ok()
}

/// Fingerprint of the certificate the peer presented, if any.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<Fingerprint> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(|c| Fingerprint::of(c.as_ref()))
}

/// Server name used in the handshake. It is not verified (the certificate is pinned instead).
pub const SERVER_NAME: &str = "umac-host";
