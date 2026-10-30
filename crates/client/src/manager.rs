//! Owns the connections to paired hosts: one session per host, connected on first use and
//! reconnected on the next request after it drops. Requests (open, launch, desktop, lists)
//! go to the host's session; UI commands are routed to the session of their window.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use umac_proto::{DisplayInfo, LaunchAction, WindowId, WindowInfo, WindowKind};

use crate::{
    config::{Config, HostEntry, Store},
    ids::{HostKey, IdMap, Route},
    net::{self, HostCtx, HostReq, NetCmd},
    ui::{UiEvent, UiSender},
};

/// A host that failed to connect is not tried again sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(1);
/// Longest wait for a host's answer; launches wait up to 5 s for the app's window.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(12);

struct Link {
    host: HostEntry,
    req: mpsc::UnboundedSender<HostReq>,
    cmd: mpsc::UnboundedSender<NetCmd>,
}

/// A window shown here, for `umac status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenWindow {
    pub host: String,
    pub mac_id: WindowId,
    pub kind: String,
    pub title: String,
}

#[derive(Clone)]
pub struct Manager(Arc<Inner>);

struct Inner {
    store: Store,
    cfg: Config,
    ui: UiSender,
    ids: Arc<Mutex<IdMap>>,
    links: Mutex<HashMap<HostKey, Link>>,
    /// Serializes connecting, so two requests do not open two sessions to one host.
    connecting: tokio::sync::Mutex<()>,
    failed: Mutex<HashMap<HostKey, Instant>>,
}

impl Manager {
    /// Returns the manager and the sender the UI core sends its commands to.
    pub fn new(store: Store, cfg: Config, ui: UiSender) -> (Self, mpsc::UnboundedSender<NetCmd>) {
        let manager = Manager(Arc::new(Inner {
            store,
            cfg,
            ui,
            ids: Arc::default(),
            links: Mutex::default(),
            connecting: tokio::sync::Mutex::new(()),
            failed: Mutex::default(),
        }));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(manager.clone().route(rx));
        (manager, tx)
    }

    /// Forwards UI commands (local window ids) to the session of each window's host.
    async fn route(self, mut rx: mpsc::UnboundedReceiver<NetCmd>) {
        while let Some(cmd) = rx.recv().await {
            let links = self.0.links.lock().unwrap();
            let send = |host: &HostKey, cmd: NetCmd| {
                if let Some(l) = links.get(host) {
                    let _ = l.cmd.send(cmd);
                }
            };
            match cmd {
                NetCmd::Control(msg) => {
                    if let Some((host, msg)) = self.0.ids.lock().unwrap().route_control(msg) {
                        send(&host, NetCmd::Control(msg));
                    }
                }
                NetCmd::Input(ev) => match self.0.ids.lock().unwrap().route_input(ev) {
                    Route::To(host, ev) => send(&host, NetCmd::Input(ev)),
                    Route::All(ev) => {
                        for l in links.values() {
                            let _ = l.cmd.send(NetCmd::Input(ev.clone()));
                        }
                    }
                    Route::Drop => {}
                },
                // Decoders report straight to their session.
                NetCmd::DecodeError(_) => {}
            }
        }
    }

    /// The session for `host` (the selected host when `None`), connecting if needed.
    async fn link(&self, host: Option<&str>) -> Result<(HostEntry, mpsc::UnboundedSender<HostReq>)> {
        let entry = self.0.store.host(host)?;
        let key: HostKey = entry.fingerprint.as_str().into();
        let _guard = self.0.connecting.lock().await;
        if let Some(l) = self.0.links.lock().unwrap().get(&key)
            && !l.req.is_closed()
        {
            return Ok((l.host.clone(), l.req.clone()));
        }
        if let Some(&at) = self.0.failed.lock().unwrap().get(&key)
            && at.elapsed() < RETRY_AFTER
        {
            bail!("{} was just unreachable; try again in a moment", entry.name);
        }
        let session = match net::connect(&self.0.store, &entry, None).await {
            Ok(s) => s,
            Err(e) => {
                self.0.failed.lock().unwrap().insert(key, Instant::now());
                return Err(e.context(format!("connecting to {}", entry.name)));
            }
        };
        tracing::info!(host = %entry.name, "connected");
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let ctx = HostCtx { key: key.clone(), ids: self.0.ids.clone(), ui: self.0.ui.clone(), cfg: self.0.cfg.clone() };
        self.0.links.lock().unwrap().insert(key.clone(), Link { host: entry.clone(), req: req_tx.clone(), cmd: cmd_tx.clone() });
        let me = self.clone();
        let name = entry.name.clone();
        tokio::spawn(async move {
            match net::run_host(session, ctx, req_rx, cmd_tx, cmd_rx).await {
                Ok(()) => tracing::info!(host = %name, "session closed"),
                Err(e) => tracing::warn!(host = %name, "session ended: {e:#}"),
            }
            // The session's request receiver is gone now; drop the link unless a newer one
            // already replaced it.
            let mut links = me.0.links.lock().unwrap();
            if links.get(&key).is_some_and(|l| l.req.is_closed()) {
                links.remove(&key);
            }
        });
        Ok((entry, req_tx))
    }

    async fn ask<T>(&self, host: Option<&str>, make: impl FnOnce(oneshot::Sender<Result<T>>) -> HostReq) -> Result<T> {
        let (entry, req) = self.link(host).await?;
        let (tx, rx) = oneshot::channel();
        req.send(make(tx)).map_err(|_| anyhow!("the session to {} just ended; try again", entry.name))?;
        tokio::time::timeout(ANSWER_TIMEOUT, rx)
            .await
            .context("the host did not answer in time")?
            .map_err(|_| anyhow!("the session to {} ended", entry.name))?
    }

    /// Streams windows by id or app; ones already shown come to the front instead.
    pub async fn open(&self, host: Option<&str>, windows: Vec<WindowId>, apps: Vec<String>) -> Result<String> {
        anyhow::ensure!(!windows.is_empty() || !apps.is_empty(), "give at least one window or app");
        self.ask(host, |reply| HostReq::Open { windows, apps, reply }).await
    }

    pub async fn launch(&self, host: Option<&str>, action: LaunchAction) -> Result<String> {
        self.ask(host, |reply| HostReq::Launch { action, reply }).await
    }

    pub async fn desktop(&self, host: Option<&str>, display: Option<u32>, bitrate_kbps: Option<u32>) -> Result<String> {
        self.ask(host, |reply| HostReq::Desktop { display, bitrate_kbps, reply }).await
    }

    pub async fn list(&self, host: Option<&str>) -> Result<Vec<WindowInfo>> {
        self.ask(host, |reply| HostReq::List { reply }).await
    }

    pub async fn displays(&self, host: Option<&str>) -> Result<Vec<DisplayInfo>> {
        self.ask(host, |reply| HostReq::Displays { reply }).await
    }

    /// Windows shown here, by host name.
    pub fn status(&self) -> Vec<OpenWindow> {
        let names: HashMap<HostKey, String> =
            self.0.links.lock().unwrap().iter().map(|(k, l)| (k.clone(), l.host.name.clone())).collect();
        let mut out: Vec<OpenWindow> = self
            .0
            .ids
            .lock()
            .unwrap()
            .shown()
            .map(|(_, s)| OpenWindow {
                host: names.get(&s.host).cloned().unwrap_or_else(|| s.host.to_string()),
                mac_id: s.mac,
                kind: kind_name(s.kind).into(),
                title: s.title.clone(),
            })
            .collect();
        out.sort_by(|a, b| (&a.host, a.mac_id).cmp(&(&b.host, b.mac_id)));
        out
    }

    /// Closes a shown window as its close button would; `window` is the Mac id from `status`.
    pub fn close(&self, host: Option<&str>, window: WindowId) -> Result<String> {
        let entry = self.0.store.host(host)?;
        let key: HostKey = entry.fingerprint.as_str().into();
        let local = self.0.ids.lock().unwrap().find(&key, window).with_context(|| format!("window {window} is not shown here"))?;
        self.0.ui.send(UiEvent::CloseRequest(local));
        Ok(format!("closing window {window}"))
    }

    /// Ends every session cleanly, so the hosts stop streaming at once.
    pub async fn shutdown(&self) {
        let links: Vec<mpsc::UnboundedSender<HostReq>> = self.0.links.lock().unwrap().values().map(|l| l.req.clone()).collect();
        for req in &links {
            let _ = req.send(HostReq::Close);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while links.iter().any(|r| !r.is_closed()) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn kind_name(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::Normal => "window",
        WindowKind::Popup => "popup",
        WindowKind::Desktop => "desktop",
        WindowKind::Dialog => "dialog",
    }
}
