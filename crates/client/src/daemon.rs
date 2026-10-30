//! `umac daemon`: the tray. One instance per user (file lock). Every action runs as its own
//! `umac` process, so a crashing stream never takes the tray down.

use std::{
    fs::File,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use crate::{
    config::Store,
    tray::{self, Action, Menu, Tray},
};

/// How often the menu picks up hosts paired or selected from the command line.
const REFRESH: Duration = Duration::from_secs(2);
/// A repeat of the same tray action within this window is ignored.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// Stream processes started from the tray at once.
const MAX_CHILDREN: usize = 16;
/// Set on processes started from the tray: they report failures as desktop notifications.
pub const NOTIFY_ENV: &str = "UMAC_NOTIFY";

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

/// The `umac` arguments that perform a tray action against host `host`.
pub fn action_args(action: &Action, host: &str) -> Option<Vec<String>> {
    let launch = |what: &str| vec!["launch".into(), what.into(), "--host".into(), host.into()];
    Some(match action {
        Action::Apps => launch("apps"),
        Action::Terminal => launch("terminal"),
        Action::TaskManager => launch("activity-monitor"),
        Action::Desktop => vec!["desktop".into(), "--host".into(), host.into()],
        Action::SelectHost(_) | Action::Exit => return None,
    })
}

fn menu(store: &Store) -> Menu {
    let hosts = store.hosts().unwrap_or_default();
    let selected = store.host(None).ok();
    Menu::new(&hosts, selected.as_ref())
}

pub async fn run(store: Store, config_dir: Option<PathBuf>) -> Result<()> {
    let Some(_lock) = try_lock(&lock_dir(store.dir()))? else {
        println!("umac is already running in the tray");
        return Ok(());
    };
    let cfg = store.config()?;
    let kdeglobals = dirs::config_dir().and_then(|d| std::fs::read_to_string(d.join("kdeglobals")).ok());
    let icon = tray::icon::pixmaps(tray::icon::line_color(cfg.tray_icon_color, kdeglobals.as_deref()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut shown = menu(&store);
    let tray = Tray::spawn(icon, shown.clone(), tx).await?;
    tracing::info!("tray ready");
    let exe = std::env::current_exe()?;
    let mut children: Vec<Child> = Vec::new();
    let mut last: Option<(Action, Instant)> = None;
    let mut tick = tokio::time::interval(REFRESH);
    loop {
        tokio::select! {
            Some(action) = rx.recv() => match action {
                Action::Exit => break,
                Action::SelectHost(fp) => {
                    if let Err(e) = store.select_host(&fp) {
                        tracing::warn!("saving the selected host: {e:#}");
                    }
                    shown = menu(&store);
                    tray.set_menu(shown.clone()).await;
                }
                action => {
                    // Any process on the session bus can trigger tray actions; a burst must not
                    // turn into a pile of processes.
                    children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
                    let now = Instant::now();
                    if last.as_ref().is_some_and(|(a, t)| *a == action && now.duration_since(*t) < DEBOUNCE)
                        || children.len() >= MAX_CHILDREN
                    {
                        tracing::info!(?action, children = children.len(), "ignoring repeated tray action");
                        continue;
                    }
                    last = Some((action.clone(), now));
                    let Ok(host) = store.host(None) else {
                        tracing::warn!("no host selected; pick one under Paired hosts");
                        continue;
                    };
                    let Some(args) = action_args(&action, &host.name) else { continue };
                    tracing::info!(?action, host = %host.name, "starting");
                    let child = Command::new(&exe)
                        .args(base_args(config_dir.as_deref()))
                        .args(&args)
                        .env(NOTIFY_ENV, "1")
                        .stdin(Stdio::null())
                        .spawn();
                    match child {
                        Ok(c) => children.push(c),
                        Err(e) => tracing::warn!("starting {args:?}: {e}"),
                    }
                }
            },
            _ = tick.tick() => {
                children.retain_mut(|c| match c.try_wait() {
                    Ok(Some(status)) => {
                        if !status.success() {
                            tracing::info!(pid = c.id(), %status, "action exited");
                        }
                        false
                    }
                    Ok(None) => true,
                    Err(_) => false,
                });
                let fresh = menu(&store);
                if fresh != shown {
                    shown = fresh;
                    tray.set_menu(shown.clone()).await;
                }
            }
        }
    }
    tray.shutdown().await;
    tracing::info!(running = children.len(), "tray closed; open windows keep running");
    Ok(())
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

    #[test]
    fn actions_map_to_commands() {
        assert_eq!(action_args(&Action::Apps, "mac-a").unwrap(), ["launch", "apps", "--host", "mac-a"]);
        assert_eq!(action_args(&Action::TaskManager, "mac-a").unwrap()[1], "activity-monitor");
        assert_eq!(action_args(&Action::Desktop, "m").unwrap(), ["desktop", "--host", "m"]);
        assert!(action_args(&Action::Exit, "m").is_none());
        assert_eq!(base_args(Some(Path::new("/c"))), ["--config-dir", "/c"]);
    }
}
