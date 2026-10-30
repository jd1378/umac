//! `umac daemon`: one process per user (file lock) that owns every stream window, the tray,
//! the host sessions and the control socket the `umac` commands talk to.

use std::{
    fs::File,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use umac_proto::LaunchAction;

use crate::{
    config::Store,
    core::Core,
    ipc::{self, Reply, Request},
    manager::Manager,
    shell::{Backend, Shell},
    tray::{self, Action, Menu, Tray},
    ui::{UiEvent, UiSender},
};

/// How often the menu picks up hosts paired or selected from the command line.
const REFRESH: Duration = Duration::from_secs(2);
/// A repeat of the same tray action within this window is ignored.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// How long a command waits for a daemon it started to listen.
const START_WAIT: Duration = Duration::from_secs(5);

/// Held while the daemon runs; the lock is released when the file closes.
pub struct InstanceLock(#[allow(dead_code)] File);

/// Directory for the lock: `$XDG_RUNTIME_DIR/umac`, else `fallback`.
pub fn lock_dir(fallback: &Path) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map_or(fallback.to_path_buf(), |d| PathBuf::from(d).join("umac"))
}

/// Takes the single-instance lock, or `None` if another daemon holds it.
pub fn try_lock(dir: &Path) -> Result<Option<InstanceLock>> {
    umac_transport::identity::create_private_dir(dir)?;
    let path = dir.join("daemon.lock");
    let file = File::options().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(InstanceLock(file)));
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(err).context("locking the daemon lock file")
}

pub fn is_running(dir: &Path) -> Result<bool> {
    Ok(try_lock(dir)?.is_none())
}

/// The arguments that select the same config directory in a child `umac`.
fn base_args(config_dir: Option<&Path>) -> Vec<String> {
    config_dir.map_or(Vec::new(), |d| vec!["--config-dir".into(), d.to_string_lossy().into_owned()])
}

/// Starts `umac daemon` in the background, detached from this terminal, logging to the state dir.
pub fn spawn_detached(config_dir: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log_dir = dirs::state_dir().or_else(dirs::cache_dir).context("no state directory")?.join("umac");
    umac_transport::identity::create_private_dir(&log_dir)?;
    let log = File::options().create(true).append(true).open(log_dir.join("daemon.log"))?;
    let mut cmd = Command::new(exe);
    cmd.args(base_args(config_dir)).arg("daemon").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    use std::os::unix::process::CommandExt;
    // A new session, so closing the terminal does not take the daemon down with it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting the daemon")?;
    Ok(())
}

/// Sends `req` to the daemon, starting it first if it is not running.
pub async fn request(store: &Store, config_dir: Option<&Path>, req: &Request) -> Result<Reply> {
    let dir = lock_dir(store.dir());
    match ipc::call(&dir, req).await {
        Ok(reply) => return Ok(reply),
        Err(e) if is_running(&dir)? => return Err(e.context("the daemon is running but did not answer")),
        Err(_) => {}
    }
    spawn_detached(config_dir)?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        match ipc::call(&dir, req).await {
            Ok(reply) => return Ok(reply),
            Err(e) if Instant::now() >= deadline => {
                return Err(e.context("the daemon did not start; see ~/.local/state/umac/daemon.log"));
            }
            Err(_) => {}
        }
    }
}

/// Ends the shell when the process is asked to stop (SIGTERM, Ctrl+C).
pub async fn shutdown_on_signal(ui: UiSender) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    ui.send(UiEvent::Shutdown(String::new()));
}

fn menu(store: &Store) -> Menu {
    let hosts = store.hosts().unwrap_or_default();
    let selected = store.host(None).ok();
    Menu::new(&hosts, selected.as_ref())
}

/// Runs the daemon on this thread (the Wayland event loop) until Exit, `umac quit` or a signal.
pub fn run(rt: &tokio::runtime::Runtime, store: Store) -> Result<()> {
    let dir = lock_dir(store.dir());
    let Some(_lock) = try_lock(&dir)? else {
        println!("umac is already running in the tray");
        return Ok(());
    };
    let cfg = store.config()?;
    let shell = Shell::new(Backend::Wayland).context("the umac daemon needs a Wayland session")?;
    let ui = shell.sender();
    let _rt = rt.enter();
    let (manager, cmd_tx) = Manager::new(store.clone(), cfg.clone(), ui.clone());
    let listener = ipc::bind(&dir)?;
    rt.spawn(ipc::serve(listener, {
        let (manager, store, ui) = (manager.clone(), store.clone(), ui.clone());
        move |req| handle(req, manager.clone(), store.clone(), ui.clone())
    }));
    rt.spawn(tray_loop(store.clone(), cfg.tray_icon_color, manager.clone(), ui.clone()));
    rt.spawn(shutdown_on_signal(ui));
    tracing::info!("daemon ready");
    let result = shell.run(Core::new(cfg, cmd_tx).persistent());
    rt.block_on(manager.shutdown());
    let _ = std::fs::remove_file(ipc::socket_path(&dir));
    tracing::info!("daemon stopped");
    match result? {
        Some(e) => bail!(e),
        None => Ok(()),
    }
}

async fn handle(req: Request, manager: Manager, store: Store, ui: UiSender) -> Reply {
    let done = |r: Result<String>| match r {
        Ok(message) => Reply::Done { message },
        Err(e) => Reply::Error { message: format!("{e:#}") },
    };
    match req {
        Request::Open { host, windows, apps } => done(manager.open(host.as_deref(), windows, apps).await),
        Request::Launch { host, action } => done(manager.launch(host.as_deref(), action).await),
        Request::Desktop { host, display, bitrate_kbps } => done(manager.desktop(host.as_deref(), display, bitrate_kbps).await),
        Request::List { host } => manager.list(host.as_deref()).await.map(|windows| Reply::Windows { windows }).unwrap_or_else(err),
        Request::Displays { host } => manager.displays(host.as_deref()).await.map(|displays| Reply::Displays { displays }).unwrap_or_else(err),
        Request::Hosts => store.hosts().map(|hosts| Reply::Hosts { hosts }).unwrap_or_else(err),
        Request::Status => Reply::Status { windows: manager.status() },
        Request::Close { host, window } => done(manager.close(host.as_deref(), window)),
        Request::Quit => {
            ui.send(UiEvent::Shutdown(String::new()));
            Reply::Done { message: "umac stopped".into() }
        }
    }
}

fn err(e: anyhow::Error) -> Reply {
    Reply::Error { message: format!("{e:#}") }
}

/// The tray: menu clicks act through the manager in this process. Exit stops the daemon.
async fn tray_loop(store: Store, color: crate::config::TrayIconColor, manager: Manager, ui: UiSender) {
    let kdeglobals = dirs::config_dir().and_then(|d| std::fs::read_to_string(d.join("kdeglobals")).ok());
    let icon = tray::icon::pixmaps(tray::icon::line_color(color, kdeglobals.as_deref()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut shown = menu(&store);
    let tray = match Tray::spawn(icon, shown.clone(), tx).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("no tray icon: {e:#}");
            return;
        }
    };
    let mut last: Option<(Action, Instant)> = None;
    let mut tick = tokio::time::interval(REFRESH);
    loop {
        tokio::select! {
            Some(action) = rx.recv() => match action {
                Action::Exit => {
                    ui.send(UiEvent::Shutdown(String::new()));
                    break;
                }
                Action::SelectHost(fp) => {
                    if let Err(e) = store.select_host(&fp) {
                        tracing::warn!("saving the selected host: {e:#}");
                    }
                    shown = menu(&store);
                    tray.set_menu(shown.clone()).await;
                }
                action => {
                    // Any process on the session bus can trigger tray actions; ignore bursts.
                    let now = Instant::now();
                    if last.as_ref().is_some_and(|(a, t)| *a == action && now.duration_since(*t) < DEBOUNCE) {
                        tracing::info!(?action, "ignoring repeated tray action");
                        continue;
                    }
                    last = Some((action.clone(), now));
                    let manager = manager.clone();
                    tokio::spawn(async move {
                        let result = match action {
                            Action::Apps => manager.launch(None, LaunchAction::Apps).await,
                            Action::Terminal => manager.launch(None, LaunchAction::Terminal).await,
                            Action::TaskManager => manager.launch(None, LaunchAction::ActivityMonitor).await,
                            Action::Settings => manager.launch(None, LaunchAction::SystemSettings).await,
                            Action::Desktop => manager.desktop(None, None, None).await,
                            Action::SelectHost(_) | Action::Exit => return,
                        };
                        if let Err(e) = result {
                            tracing::warn!(?action, "{e:#}");
                            notify("umac", &format!("{e:#}")).await;
                        }
                    });
                }
            },
            _ = tick.tick() => {
                let fresh = menu(&store);
                if fresh != shown {
                    shown = fresh;
                    tray.set_menu(shown.clone()).await;
                }
            }
        }
    }
    tray.shutdown().await;
}

/// Shows a desktop notification (org.freedesktop.Notifications). Best effort.
pub async fn notify(summary: &str, body: &str) {
    let result = async {
        let conn = zbus::Connection::session().await?;
        let hints = std::collections::HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("umac", 0u32, "umac", summary, body, Vec::<&str>::new(), hints, 8000i32),
        )
        .await?;
        zbus::Result::Ok(())
    };
    match tokio::time::timeout(Duration::from_secs(2), result).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!("notification: {e}"),
        Err(_) => tracing::debug!("notification timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_lock() {
        let dir = std::env::temp_dir().join(format!("umac-lock-{}", std::process::id()));
        let first = try_lock(&dir).unwrap();
        assert!(first.is_some());
        assert!(try_lock(&dir).unwrap().is_none());
        assert!(is_running(&dir).unwrap());
        drop(first);
        assert!(!is_running(&dir).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
