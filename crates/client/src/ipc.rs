//! Local control socket of the daemon: `umac open`, `launch`, `desktop`, `list` and friends
//! are one JSON line request and one JSON line reply. Only the same user may connect: the
//! socket lives in a 0700 directory, is 0600 itself, and each peer's uid is checked.

use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use umac_proto::{DisplayInfo, LaunchAction, WindowId, WindowInfo};

use crate::{config::HostEntry, manager::OpenWindow};

pub const VERSION: u32 = 1;
/// Longest request or reply line; window lists are the largest.
const MAX_LINE: u64 = 1 << 20;
/// A client that sends nothing is dropped after this.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Launches wait for the app's window, so replies can take a while.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Open { host: Option<String>, windows: Vec<WindowId>, apps: Vec<String> },
    Launch { host: Option<String>, action: LaunchAction },
    Desktop { host: Option<String>, display: Option<u32>, bitrate_kbps: Option<u32> },
    List { host: Option<String> },
    Displays { host: Option<String> },
    /// Closes a shown window (Mac id) like its close button.
    Close { host: Option<String>, window: WindowId },
    Hosts,
    Status,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Done { message: String },
    Windows { windows: Vec<WindowInfo> },
    Displays { displays: Vec<DisplayInfo> },
    Hosts { hosts: Vec<HostEntry> },
    Status { windows: Vec<OpenWindow> },
    Error { message: String },
}

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    version: u32,
    body: T,
}

pub fn encode<T: Serialize>(body: &T) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(&Envelope { version: VERSION, body })?;
    anyhow::ensure!(line.len() as u64 <= MAX_LINE, "message too large");
    line.push(b'\n');
    Ok(line)
}

pub fn decode<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Result<T> {
    anyhow::ensure!(line.len() as u64 <= MAX_LINE, "message too large");
    let env: Envelope<T> = serde_json::from_slice(line).context("malformed message")?;
    if env.version != VERSION {
        bail!("umac versions differ (message v{}, this is v{VERSION}); restart the daemon with `umac quit`", env.version);
    }
    Ok(env.body)
}

/// Only processes of the same user may control the daemon.
pub fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("daemon.sock")
}

/// Binds the socket in `dir` (created 0700). Call only while holding the instance lock, which
/// makes removing a stale socket safe.
pub fn bind(dir: &Path) -> Result<UnixListener> {
    umac_transport::identity::create_private_dir(dir)?;
    let path = socket_path(dir);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn read_line(stream: &mut BufReader<UnixStream>) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let n = (&mut *stream).take(MAX_LINE + 1).read_until(b'\n', &mut line).await?;
    anyhow::ensure!(n > 0, "connection closed");
    anyhow::ensure!(line.ends_with(b"\n"), "message too large or cut off");
    line.pop();
    Ok(line)
}

/// Serves requests until the listener fails; each connection is handled on its own task.
pub async fn serve<F, Fut>(listener: UnixListener, handle: F)
where
    F: Fn(Request) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Reply> + Send,
{
    let own = unsafe { libc::getuid() };
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                tracing::warn!("control socket: {e}");
                return;
            }
        };
        match stream.peer_cred() {
            Ok(c) if peer_allowed(c.uid(), own) => {}
            Ok(c) => {
                tracing::warn!(uid = c.uid(), "refused a control connection from another user");
                continue;
            }
            Err(e) => {
                tracing::warn!("control socket peer: {e}");
                continue;
            }
        }
        let handle = handle.clone();
        tokio::spawn(async move {
            let mut stream = BufReader::new(stream);
            let reply = match tokio::time::timeout(READ_TIMEOUT, read_line(&mut stream)).await {
                Ok(Ok(line)) => match decode::<Request>(&line) {
                    Ok(req) => {
                        tracing::info!(?req, "control request");
                        handle(req).await
                    }
                    Err(e) => Reply::Error { message: format!("{e:#}") },
                },
                Ok(Err(e)) => Reply::Error { message: format!("{e:#}") },
                Err(_) => return,
            };
            if let Ok(line) = encode(&reply) {
                let _ = stream.get_mut().write_all(&line).await;
            }
        });
    }
}

/// Sends one request to the daemon listening in `dir`.
pub async fn call(dir: &Path, req: &Request) -> Result<Reply> {
    let stream = UnixStream::connect(socket_path(dir)).await?;
    let mut stream = BufReader::new(stream);
    stream.get_mut().write_all(&encode(req)?).await?;
    let line = tokio::time::timeout(REPLY_TIMEOUT, read_line(&mut stream)).await.context("the daemon did not answer in time")??;
    decode(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_version_check() {
        let req = Request::Launch { host: Some("mac-a".into()), action: LaunchAction::Terminal };
        let line = encode(&req).unwrap();
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(decode::<Request>(&line[..line.len() - 1]).unwrap(), req);
        let other = br#"{"version":99,"body":{"type":"quit"}}"#;
        assert!(decode::<Request>(other).unwrap_err().to_string().contains("versions differ"));
        assert!(decode::<Request>(b"{nonsense").is_err());
        let reply = Reply::Error { message: "x".into() };
        let line = encode(&reply).unwrap();
        assert_eq!(decode::<Reply>(&line[..line.len() - 1]).unwrap(), reply);
    }

    #[test]
    fn caps_size() {
        let huge = vec![b'a'; MAX_LINE as usize + 1];
        assert!(decode::<Request>(&huge).is_err());
        let req = Request::Open { host: None, windows: vec![], apps: vec!["a".repeat(MAX_LINE as usize)] };
        assert!(encode(&req).is_err());
    }

    #[test]
    fn only_the_same_user() {
        assert!(peer_allowed(1000, 1000));
        assert!(!peer_allowed(0, 1000));
        assert!(!peer_allowed(1001, 1000));
    }

    #[tokio::test]
    async fn socket_request_reply() {
        let dir = std::env::temp_dir().join(format!("umac-ipc-{}", std::process::id()));
        let listener = bind(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(socket_path(&dir)).unwrap().permissions().mode() & 0o777, 0o600);
        tokio::spawn(serve(listener, |req| async move {
            match req {
                Request::Status => Reply::Status { windows: vec![] },
                _ => Reply::Error { message: "no".into() },
            }
        }));
        assert_eq!(call(&dir, &Request::Status).await.unwrap(), Reply::Status { windows: vec![] });
        assert_eq!(call(&dir, &Request::Hosts).await.unwrap(), Reply::Error { message: "no".into() });
        std::fs::remove_dir_all(dir).unwrap();
    }
}
