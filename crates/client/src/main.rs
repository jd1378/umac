use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use umac_client::{
    config::{HostEntry, PairingLink, Store},
    core::Core,
    net::{self, Targets},
    shell::{Backend, Shell},
    ui::UiEvent,
};

#[derive(Parser)]
#[command(name = "umac", about = "Show individual Mac windows as native windows on this PC")]
struct Cli {
    /// Directory for identity, paired hosts and settings (default: ~/.config/umac).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
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
    },
    /// Stream windows; each opens as its own local window.
    Open {
        #[arg(long)]
        host: Option<String>,
        /// Window id from `umac list` (repeatable).
        #[arg(long = "window")]
        windows: Vec<u32>,
        /// Every normal window of an app, by name or bundle id (repeatable).
        #[arg(long = "app")]
        apps: Vec<String>,
        /// Windowing backend.
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
    },
    /// List the host's displays.
    Displays {
        #[arg(long)]
        host: Option<String>,
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
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let store = Store::new(cli.config_dir)?;
    let rt = tokio::runtime::Runtime::new()?;
    match cli.cmd {
        Cmd::Pair { link, name } => {
            let link: PairingLink = link.parse()?;
            let mut entry =
                HostEntry { name: String::new(), addr: link.addr, fingerprint: link.fingerprint.to_string() };
            let session = rt.block_on(net::connect(&store, &entry, Some(link.token)))?;
            entry.name = name.unwrap_or(session.host_name);
            store.save_host(entry.clone())?;
            println!("paired with {} at {}", entry.name, entry.addr);
        }
        Cmd::Hosts => {
            for h in store.hosts()? {
                println!("{}\t{}\t{}", h.name, h.addr, h.fingerprint);
            }
        }
        Cmd::List { host } => {
            let entry = store.host(host.as_deref())?;
            let windows = rt.block_on(async {
                let mut s = net::connect(&store, &entry, None).await?;
                s.list_windows().await
            })?;
            println!("{:>8}  {:<24} {:>11}  title", "id", "app", "size");
            for w in windows {
                let size = format!("{}x{}", w.width, w.height);
                println!("{:>8}  {:<24} {:>11}  {}", w.id, w.app_name, size, w.title);
            }
        }
        Cmd::Displays { host } => {
            let entry = store.host(host.as_deref())?;
            let displays = rt.block_on(async {
                let mut s = net::connect(&store, &entry, None).await?;
                s.list_displays().await
            })?;
            println!("{:>5}  {:>11}  {:>5}  name", "index", "pixels", "scale");
            for d in displays {
                let size = format!("{}x{}", d.width, d.height);
                println!("{:>5}  {:>11}  {:>5}  {}{}", d.index, size, d.scale, d.name, if d.main { " (main)" } else { "" });
            }
        }
        Cmd::Desktop { host, display, bitrate, backend } => {
            let entry = store.host(host.as_deref())?;
            let index = match display {
                Some(i) => i,
                None => rt.block_on(async {
                    let mut s = net::connect(&store, &entry, None).await?;
                    let list = s.list_displays().await?;
                    anyhow::Ok(list.iter().find(|d| d.main).or(list.first()).context("host has no displays")?.index)
                })?,
            };
            let targets = Targets { displays: vec![index], desktop_bitrate_kbps: bitrate, ..Default::default() };
            run_streams(&rt, store, entry, targets, backend)?;
        }
        Cmd::Open { host, windows, apps, backend } => {
            anyhow::ensure!(!windows.is_empty() || !apps.is_empty(), "give at least one --window or --app");
            let entry = store.host(host.as_deref())?;
            run_streams(&rt, store, entry, Targets { windows, apps, ..Default::default() }, backend)?;
        }
    }
    Ok(())
}

/// Streams `targets` until every window is closed.
fn run_streams(rt: &tokio::runtime::Runtime, store: Store, entry: HostEntry, targets: Targets, backend: Backend) -> Result<()> {
    let cfg = store.config()?;
    let shell = Shell::new(backend)?;
    let ui = shell.sender();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let net_cmd_tx = cmd_tx.clone();
    let net_cfg = cfg.clone();
    rt.spawn(async move {
        let result = async {
            let session = net::connect(&store, &entry, None).await?;
            net::run_open(session, targets, net_cfg, ui.clone(), net_cmd_tx, cmd_rx).await
        }
        .await;
        let reason = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
        ui.send(UiEvent::Shutdown(reason));
    });
    if let Some(e) = shell.run(Core::new(cfg, cmd_tx)).context("window system")? {
        anyhow::bail!(e);
    }
    Ok(())
}
