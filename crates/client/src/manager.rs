//! Owns the connections to paired hosts: one session per host, connected on first use and
//! reconnected on the next request after it drops. Requests (open, launch, desktop, lists)
//! go to the host's session; UI commands are routed to the session of their window.
//!
//! A Mac with several users has one agent per user, and only the agent of the user on its
//! screen can stream. Requests without an explicit host go to whichever agent of the selected
//! Mac has its user on screen.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use umac_proto::{DisplayInfo, LaunchAction, WindowId, WindowInfo, WindowKind};

use crate::{
    config::{self, Config, HostEntry, Store},
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
    /// Mac user of the agent.
    user: String,
    /// Whether that user is on the Mac's screen; updated by the session.
    on_screen: Arc<AtomicBool>,
}

impl Link {
    fn live(&self) -> bool {
        !self.req.is_closed()
    }
}

/// Who is on a Mac's screen, as far as the open sessions tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenUser {
    /// No session to that Mac is open.
    Unknown,
    /// Connected, but none of the paired users is on screen.
    Nobody,
    User(String),
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

    /// The session for `host`. Without a name, the session of whichever paired user of the
    /// selected Mac is on its screen. Connects as needed.
    async fn link(&self, host: Option<&str>) -> Result<(HostEntry, mpsc::UnboundedSender<HostReq>)> {
        let entry = self.0.store.host(host)?;
        let group = match host {
            Some(_) => vec![entry],
            None => config::mac_group(&self.0.store.hosts()?, &entry),
        };
        self.on_screen_link(group).await
    }

    /// The live session of the first `group` agent whose user is on screen, connecting the
    /// ones without a session first. Several can be on screen at once (one at the Mac, others
    /// in Screen Sharing sessions); `group` order decides.
    async fn on_screen_link(&self, group: Vec<HostEntry>) -> Result<(HostEntry, mpsc::UnboundedSender<HostReq>)> {
        let _guard = self.0.connecting.lock().await;
        let live = |e: &HostEntry| -> Option<(bool, HostEntry, mpsc::UnboundedSender<HostReq>, String)> {
            let links = self.0.links.lock().unwrap();
            let l = links.get(e.fingerprint.as_str()).filter(|l| l.live())?;
            Some((l.on_screen.load(Ordering::Relaxed), l.host.clone(), l.req.clone(), l.user.clone()))
        };
        if let Some((true, host, req, _)) = group.first().and_then(live) {
            return Ok((host, req));
        }
        // Concurrently: an agent whose user logged out does not answer until the timeout.
        let mut tries = tokio::task::JoinSet::new();
        for e in group.iter().filter(|e| live(e).is_none()).cloned() {
            if let Some(&at) = self.0.failed.lock().unwrap().get(e.fingerprint.as_str())
                && at.elapsed() < RETRY_AFTER
            {
                tries.spawn(async move { (Err(anyhow!("{} was just unreachable; try again in a moment", e.name)), e) });
                continue;
            }
            let store = self.0.store.clone();
            tries.spawn(async move { (net::connect(&store, &e, None).await, e) });
        }
        let mut errors = Vec::new();
        while let Some(joined) = tries.join_next().await {
            let Ok((result, e)) = joined else { continue };
            match result {
                Ok(session) => self.adopt(&e, session),
                Err(err) => {
                    self.0.failed.lock().unwrap().insert(e.fingerprint.as_str().into(), Instant::now());
                    errors.push(err.context(format!("connecting to {}", e.name)));
                }
            }
        }
        let mut off_screen = Vec::new();
        for e in &group {
            match live(e) {
                Some((true, host, req, _)) => return Ok((host, req)),
                Some((false, _, _, user)) => off_screen.push(user),
                None => {}
            }
        }
        let mac = group.first().map_or("the Mac", |e| e.mac_name()).to_owned();
        match off_screen.as_slice() {
            [] => Err(errors.into_iter().next().unwrap_or_else(|| anyhow!("no paired host"))),
            [user] => bail!("{user} is not on {mac}'s screen; switch to {user} at the Mac or log in as {user} with Screen Sharing"),
            users => bail!(
                "none of the paired users ({}) is on {mac}'s screen; switch to one at the Mac or log in as one with Screen Sharing",
                users.join(", ")
            ),
        }
    }

    /// Starts the session task for a fresh connection and records it as the link of `entry`.
    fn adopt(&self, entry: &HostEntry, session: net::Session) {
        let key: HostKey = entry.fingerprint.as_str().into();
        let mac = session.mac.clone();
        if let Err(e) = self.0.store.learn(&entry.fingerprint, &mac.user, &mac.machine) {
            tracing::warn!(host = %entry.name, "saving the Mac user: {e:#}");
        }
        tracing::info!(host = %entry.name, user = %mac.user, on_screen = mac.on_screen, "connected");
        let on_screen = Arc::new(AtomicBool::new(mac.on_screen));
        let screen = {
            let (me, flag, fingerprint, name) = (self.clone(), on_screen.clone(), entry.fingerprint.clone(), entry.name.clone());
            Box::new(move |now: bool| {
                flag.store(now, Ordering::Relaxed);
                tracing::info!(host = %name, on_screen = now, "Mac user {}", if now { "is back on screen" } else { "switched away" });
                // Find out who took the screen, so the tray shows it and the next request is fast.
                if !now {
                    let (me, fingerprint) = (me.clone(), fingerprint.clone());
                    tokio::spawn(async move { me.follow_switch(&fingerprint).await });
                }
            })
        };
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let ctx = HostCtx { key: key.clone(), ids: self.0.ids.clone(), ui: self.0.ui.clone(), cfg: self.0.cfg.clone(), screen };
        let link = Link { host: entry.clone(), req: req_tx.clone(), cmd: cmd_tx.clone(), user: mac.user, on_screen };
        self.0.links.lock().unwrap().insert(key.clone(), link);
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
            if links.get(&key).is_some_and(|l| !l.live()) {
                links.remove(&key);
            }
        });
    }

    /// The user of the agent `fingerprint` left the screen: connect the Mac's other paired
    /// users to see who is on screen now.
    async fn follow_switch(&self, fingerprint: &str) {
        let Ok(hosts) = self.0.store.hosts() else { return };
        let Some(entry) = hosts.iter().find(|h| h.fingerprint == fingerprint) else { return };
        let group = config::mac_group(&hosts, entry);
        if group.len() > 1
            && let Err(e) = self.on_screen_link(group).await
        {
            tracing::info!("after the Mac user switch: {e:#}");
        }
    }

    /// The user requests for `entry`'s Mac go to: `entry`'s own when on screen, else another
    /// user of that Mac who is.
    pub fn screen_user(&self, entry: &HostEntry) -> ScreenUser {
        let links = self.0.links.lock().unwrap();
        let mut connected = false;
        let mut other = None;
        for l in links.values().filter(|l| l.live() && l.host.same_mac(entry)) {
            connected = true;
            if l.on_screen.load(Ordering::Relaxed) {
                if l.host.fingerprint == entry.fingerprint {
                    return ScreenUser::User(l.user.clone());
                }
                other.get_or_insert_with(|| l.user.clone());
            }
        }
        match other {
            Some(u) => ScreenUser::User(u),
            None if connected => ScreenUser::Nobody,
            None => ScreenUser::Unknown,
        }
    }

    /// Whether each connected agent of `entry`'s Mac can stream now, by fingerprint.
    pub fn streamable(&self, entry: &HostEntry) -> HashMap<String, bool> {
        let links = self.0.links.lock().unwrap();
        links
            .values()
            .filter(|l| l.live() && l.host.same_mac(entry))
            .map(|l| (l.host.fingerprint.clone(), l.on_screen.load(Ordering::Relaxed)))
            .collect()
    }

    /// Connects to the selected Mac in the background, so the tray can show who is on screen.
    pub async fn probe(&self) {
        if let Err(e) = self.link(None).await {
            tracing::info!("selected Mac: {e:#}");
        }
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
