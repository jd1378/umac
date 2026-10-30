use std::path::PathBuf;

use std::future::Future;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use umac_proto::{DisplayInfo, LaunchAction, WindowInfo};
use umac_client::{
    autostart,
    config::{HostEntry, PairingLink, Store},
    core::Core,
    daemon,
    ipc::{Reply, Request},
    manager::Manager,
    net,
    shell::{Backend, Shell},
    ui::{UiEvent, UiSender},
};

#[derive(Parser)]
#[command(
    name = "umac",
    about = "Show individual Mac windows as native windows on this PC",
    after_help = "Without a command, starts the tray in the background if a Mac is paired."
)]
struct Cli {
    /// Directory for identity, paired hosts and settings (default: ~/.config/umac).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Clone, Copy, ValueEnum)]
enum LaunchArg {
    /// The Mac's Apps launcher (a Finder window on /Applications where it cannot be streamed).
    Apps,
    /// A new Terminal window.
    Terminal,
    ActivityMonitor,
    /// A Finder window on /Applications.
    Finder,
}

impl From<LaunchArg> for LaunchAction {
    fn from(a: LaunchArg) -> Self {
        match a {
            LaunchArg::Apps => LaunchAction::Apps,
            LaunchArg::Terminal => LaunchAction::Terminal,
            LaunchArg::ActivityMonitor => LaunchAction::ActivityMonitor,
            LaunchArg::Finder => LaunchAction::FinderApplications,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum AutostartArg {
    Enable,
    Disable,
    Status,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with a host using the link printed by `umac-host pair`.
    Pair {
        link: String,
        /// Local name for the host (default: the name the host reports).
        #[arg(long)]
        name: Option<String>,
    },
    /// List paired hosts.
    Hosts,
    /// List windows available on the host.
    List {
        #[arg(long)]
        host: Option<String>,
        /// Ask the host directly instead of through the daemon.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Windows shown here right now (from the daemon).
    Status,
    /// Stop the daemon: closes every stream window and the tray.
    Quit,
    /// Close a shown window like its close button (and on the Mac, with close_on_mac).
    Close {
        #[arg(long)]
        host: Option<String>,
        /// Mac window id from `umac status`.
        #[arg(long)]
        window: u32,
    },
    /// Stream windows; each opens as its own local window. Already shown ones come to the front.
    Open {
        #[arg(long)]
        host: Option<String>,
        /// Window id from `umac list` (repeatable).
        #[arg(long = "window")]
        windows: Vec<u32>,
        /// Every normal window of an app, by name or bundle id (repeatable).
        #[arg(long = "app")]
        apps: Vec<String>,
        #[command(flatten)]
        standalone: Standalone,
    },
    /// Open something on the Mac (a new Terminal window, Activity Monitor...) and stream it.
    Launch {
        #[arg(value_enum)]
        what: LaunchArg,
        #[arg(long)]
        host: Option<String>,
        #[command(flatten)]
        standalone: Standalone,
    },
    /// Run the tray in the foreground (`umac` alone starts it in the background).
    Daemon,
    /// Start the tray at login, or stop doing so.
    Autostart {
        #[arg(value_enum)]
        action: AutostartArg,
    },
    /// List the host's displays.
    Displays {
        #[arg(long)]
        host: Option<String>,
        /// Ask the host directly instead of through the daemon.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Show a whole Mac display in one window (F11 toggles fullscreen).
    Desktop {
        #[arg(long)]
        host: Option<String>,
        /// Display index from `umac displays` (default: the main display).
        #[arg(long)]
        display: Option<u32>,
        /// Bitrate in kbit/s (default: scaled with the display size).
        #[arg(long)]
        bitrate: Option<u32>,
        #[command(flatten)]
        standalone: Standalone,
    },
    /// Measure latency: streams the desktop for a while, then prints a per-stage summary.
    Latency {
        #[arg(long)]
        host: Option<String>,
        /// Display index from `umac displays` (default: the main display).
        #[arg(long)]
        display: Option<u32>,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
    },
}

/// Running a stream in this process instead of the daemon (debugging, latency runs).
#[derive(clap::Args, Clone, Copy)]
struct Standalone {
    /// Show the windows from this process instead of the daemon; it ends with its last window.
    #[arg(long)]
    no_daemon: bool,
    /// Windowing backend (with --no-daemon; the daemon always uses Wayland).
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    backend: Backend,
    /// Log latency per stage every 2 s (with --no-daemon; for the daemon set show_stats).
    #[arg(long)]
    stats: bool,
}

/// Latency reporting for a streaming session.
#[derive(Clone, Copy, Default)]
struct Stats {
    show: bool,
    /// End the session after this long and print a summary (`umac latency`).
    run_for: Option<std::time::Duration>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let config_dir = cli.config_dir.clone();
    let store = Store::new(cli.config_dir)?;
    let rt = tokio::runtime::Runtime::new()?;
    run(&rt, store, config_dir, cli.cmd)
}

fn run(rt: &tokio::runtime::Runtime, store: Store, config_dir: Option<PathBuf>, cmd: Option<Cmd>) -> Result<()> {
    let Some(cmd) = cmd else {
        if store.hosts()?.is_empty() {
            println!("No Mac paired yet. Run `umac-host pair` on the Mac, then `umac pair '<link>'` here.");
        } else if daemon::is_running(&daemon::lock_dir(store.dir()))? {
            println!("umac is already running in the tray.");
        } else {
            daemon::spawn_detached(config_dir.as_deref())?;
            println!("umac started in the tray.");
        }
        return Ok(());
    };
    match cmd {
        Cmd::Pair { link, name } => {
            let link: PairingLink = link.parse()?;
            let mut entry =
                HostEntry { name: String::new(), addr: link.addr, fingerprint: link.fingerprint.to_string() };
            let session = rt.block_on(net::connect(&store, &entry, Some(link.token)))?;
            entry.name = name.unwrap_or(session.host_name);
            store.save_host(entry.clone())?;
            if store.host(None).is_err() {
                store.select_host(&entry.fingerprint)?;
            }
            println!("paired with {} at {}", entry.name, entry.addr);
            if !daemon::is_running(&daemon::lock_dir(store.dir()))? {
                daemon::spawn_detached(config_dir.as_deref())?;
                println!("umac started in the tray.");
            }
        }
        Cmd::Hosts => {
            for h in store.hosts()? {
                println!("{}\t{}\t{}", h.name, h.addr, h.fingerprint);
            }
        }
        Cmd::List { host, no_daemon } => {
            let windows = if no_daemon {
                rt.block_on(direct(&store, |m| async move { m.list(host.as_deref()).await }))?
            } else {
                match ask(rt, &store, &config_dir, Request::List { host })? {
                    Reply::Windows { windows } => windows,
                    other => bail!("unexpected reply: {other:?}"),
                }
            };
            print_windows(&windows);
        }
        Cmd::Displays { host, no_daemon } => {
            let displays = if no_daemon {
                rt.block_on(direct(&store, |m| async move { m.displays(host.as_deref()).await }))?
            } else {
                match ask(rt, &store, &config_dir, Request::Displays { host })? {
                    Reply::Displays { displays } => displays,
                    other => bail!("unexpected reply: {other:?}"),
                }
            };
            print_displays(&displays);
        }
        Cmd::Status => match ask(rt, &store, &config_dir, Request::Status)? {
            Reply::Status { windows } if windows.is_empty() => println!("no windows shown"),
            Reply::Status { windows } => {
                println!("{:<24} {:>8}  {:<8} title", "host", "mac id", "kind");
                for w in windows {
                    println!("{:<24} {:>8}  {:<8} {}", w.host, w.mac_id, w.kind, w.title);
                }
            }
            other => bail!("unexpected reply: {other:?}"),
        },
        Cmd::Close { host, window } => done(ask(rt, &store, &config_dir, Request::Close { host, window })?)?,
        Cmd::Quit => {
            let dir = daemon::lock_dir(store.dir());
            if !daemon::is_running(&dir)? {
                println!("umac is not running");
                return Ok(());
            }
            rt.block_on(umac_client::ipc::call(&dir, &Request::Quit))?;
            println!("umac stopped");
        }
        Cmd::Launch { what, host, standalone } => {
            let action = LaunchAction::from(what);
            if standalone.no_daemon {
                let stats = Stats { show: standalone.stats, ..Default::default() };
                return run_alone(rt, store, standalone.backend, stats, move |m| async move { m.launch(host.as_deref(), action).await });
            }
            done(ask(rt, &store, &config_dir, Request::Launch { host, action })?)?;
        }
        Cmd::Daemon => daemon::run(rt, store)?,
        Cmd::Autostart { action } => {
            let paths = autostart::Paths::for_user()?;
            let exe = std::env::current_exe()?;
            match action {
                AutostartArg::Enable => {
                    autostart::enable(&paths, &exe)?;
                    println!("umac starts in the tray at login ({})", paths.entry.display());
                }
                AutostartArg::Disable => match autostart::disable(&paths)? {
                    true => println!("umac no longer starts at login"),
                    false => println!("autostart was not enabled"),
                },
                AutostartArg::Status => match autostart::status(&paths, &exe)? {
                    autostart::Status::Disabled => println!("autostart: disabled"),
                    autostart::Status::Enabled { exec, current } => {
                        println!("autostart: enabled\nExec={exec}");
                        if !current {
                            println!("warning: this runs a different binary than {}; run `umac autostart enable` to update it", exe.display());
                        }
                    }
                },
            }
        }
        Cmd::Desktop { host, display, bitrate, standalone } => {
            if standalone.no_daemon {
                let stats = Stats { show: standalone.stats, ..Default::default() };
                return run_alone(rt, store, standalone.backend, stats, move |m| async move { m.desktop(host.as_deref(), display, bitrate).await });
            }
            done(ask(rt, &store, &config_dir, Request::Desktop { host, display, bitrate_kbps: bitrate })?)?;
        }
        Cmd::Latency { host, display, seconds, backend } => {
            let stats = Stats { show: true, run_for: Some(std::time::Duration::from_secs(seconds.max(1))) };
            run_alone(rt, store, backend, stats, move |m| async move { m.desktop(host.as_deref(), display, None).await })?;
            print_summary(seconds);
        }
        Cmd::Open { host, windows, apps, standalone } => {
            anyhow::ensure!(!windows.is_empty() || !apps.is_empty(), "give at least one --window or --app");
            if standalone.no_daemon {
                let stats = Stats { show: standalone.stats, ..Default::default() };
                return run_alone(rt, store, standalone.backend, stats, move |m| async move { m.open(host.as_deref(), windows, apps).await });
            }
            done(ask(rt, &store, &config_dir, Request::Open { host, windows, apps })?)?;
        }
    }
    Ok(())
}

/// Sends a request to the daemon, starting it if needed.
fn ask(rt: &tokio::runtime::Runtime, store: &Store, config_dir: &Option<PathBuf>, req: Request) -> Result<Reply> {
    rt.block_on(daemon::request(store, config_dir.as_deref(), &req))
}

fn done(reply: Reply) -> Result<()> {
    match reply {
        Reply::Done { message } => {
            println!("{message}");
            Ok(())
        }
        Reply::Error { message } => bail!(message),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Runs `f` against the host without a daemon and without windows (lists only).
async fn direct<T, F, Fut>(store: &Store, f: F) -> Result<T>
where
    F: FnOnce(Manager) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let (manager, _ui_commands) = Manager::new(store.clone(), store.config()?, UiSender::new(|_| true));
    let result = f(manager.clone()).await;
    manager.shutdown().await;
    result
}

fn print_windows(windows: &[WindowInfo]) {
    println!("{:>8}  {:<24} {:>11}  title", "id", "app", "size");
    for w in windows {
        let size = format!("{}x{}", w.width, w.height);
        println!("{:>8}  {:<24} {:>11}  {}", w.id, w.app_name, size, w.title);
    }
}

fn print_displays(displays: &[DisplayInfo]) {
    println!("{:>5}  {:>11}  {:>5}  name", "index", "pixels", "scale");
    for d in displays {
        let size = format!("{}x{}", d.width, d.height);
        println!("{:>5}  {:>11}  {:>5}  {}{}", d.index, size, d.scale, d.name, if d.main { " (main)" } else { "" });
    }
}

fn print_summary(seconds: u64) {
    let runs = umac_client::core::SUMMARY.lock().map(|s| s.clone()).unwrap_or_default();
    if runs.is_empty() {
        println!("no frames were shown; nothing to report");
        return;
    }
    let pair = |x: Option<(f64, f64)>| x.map_or("-".to_owned(), |(a, b)| format!("{a:>6.1} {b:>6.1}"));
    let one = |x: Option<f64>| x.map_or("-".to_owned(), |a| format!("{a:>6.1}"));
    for (title, r) in runs {
        println!("\n{title}: {} frames in {seconds} s (milliseconds)", r.frames);
        println!("  {:<34} {:>6} {:>6}", "stage", "p50", "p95");
        println!("  {:<34} {}", "glass to glass (Mac shown -> here)", pair(r.glass));
        println!("  {:<34} {}", "  host capture -> encoded", pair(r.encode));
        println!("  {:<34} {}", "  host encoded -> sent", one(r.send));
        println!("  {:<34} {}", "  network + reassembly", one(r.network));
        println!("  {:<34} {}", "  frame transfer (first -> last)", pair(r.transfer));
        println!("  {:<34} {}", "  decode", pair(r.decode));
        println!("  {:<34} {}", "  render and submit", pair(r.render));
        println!("  {:<34} {}", "  submit -> on screen", pair(r.display));
        println!("  clock sync uncertainty: ±{} ms", one(r.uncertainty).trim());
    }
}

/// Shows windows from this process (no daemon) until the last one closes, `stats.run_for`
/// passed, or Ctrl+C. `start` asks the manager for what to show.
fn run_alone<F, Fut>(rt: &tokio::runtime::Runtime, store: Store, backend: Backend, stats: Stats, start: F) -> Result<()>
where
    F: FnOnce(Manager) -> Fut + Send + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    let mut cfg = store.config()?;
    cfg.show_stats |= stats.show;
    cfg.latency_summary = stats.run_for.is_some();
    if let Some(after) = stats.run_for {
        std::thread::spawn(move || {
            std::thread::sleep(after);
            unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
        });
    }
    let shell = Shell::new(backend)?;
    let ui = shell.sender();
    let _rt = rt.enter();
    let (manager, cmd_tx) = Manager::new(store, cfg.clone(), ui.clone());
    rt.spawn(daemon::shutdown_on_signal(ui.clone()));
    let m = manager.clone();
    rt.spawn(async move {
        if let Err(e) = start(m).await {
            ui.send(UiEvent::Shutdown(format!("{e:#}")));
        }
    });
    let result = shell.run(Core::new(cfg, cmd_tx));
    rt.block_on(manager.shutdown());
    if let Some(e) = result? {
        bail!(e);
    }
    Ok(())
}
