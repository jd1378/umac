//! `umac-host`: streams individual macOS windows to paired clients.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

mod annexb;
mod auth;
mod clipboard;
mod clock;
mod config;
mod encode;
mod input_state;
mod popup;
#[cfg(target_os = "macos")]
mod mac;
mod status;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Stream individual macOS windows to your PC")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install the agent, request permissions and print a pairing string (safe to re-run).
    Setup,
    /// Run the streaming agent (normally started by launchd).
    Run,
    /// Print a one-time pairing string for a new client (valid 10 minutes).
    Pair,
    /// Show agent, permission and pairing state.
    Status,
    /// Stop and remove the agent.
    Uninstall {
        /// Also delete the host identity, paired clients and signing keychain.
        #[arg(long)]
        purge: bool,
    },
    /// Forget a paired client by fingerprint prefix.
    Unpair { fingerprint: String },
    /// List capturable windows.
    ListWindows {
        /// Every on-screen window with its layer (debugging popup detection).
        #[arg(long)]
        all: bool,
    },
    /// Capture and encode one window to an Annex-B file, reporting encode latency.
    CaptureTest {
        /// Window id or a substring of its app name or title.
        #[arg(long)]
        window: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        #[arg(long, default_value = "capture.h265")]
        out: PathBuf,
        #[arg(long, default_value_t = 25_000)]
        bitrate_kbps: u32,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Encode H.264 instead of HEVC.
        #[arg(long)]
        h264: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .or_else(|_| tracing_subscriber::EnvFilter::try_new(config::Settings::load().log))
                .unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if let Cmd::Unpair { fingerprint } = &cli.command {
        let n = auth::AuthStore::new(&config::support_dir()).remove_client(fingerprint)?;
        println!("removed {n} client(s)");
        return Ok(());
    }
    platform_main(cli.command)
}

#[cfg(target_os = "macos")]
fn platform_main(cmd: Cmd) -> anyhow::Result<()> {
    use mac::*;
    match cmd {
        Cmd::Setup => setup::setup(),
        Cmd::Run => tokio::runtime::Runtime::new()?.block_on(server::run(config::Settings::load())),
        Cmd::Pair => setup::print_pairing(),
        Cmd::Status => setup::status(),
        Cmd::Uninstall { purge } => setup::uninstall(purge),
        Cmd::Unpair { .. } => unreachable!(),
        Cmd::ListWindows { all: true } => {
            for w in windows::on_screen() {
                println!("{:>6} pid {:>6} layer {:>5}  {:>7.1},{:>7.1} {:>6.1}x{:<6.1}", w.id, w.pid, w.layer, w.x, w.y, w.width, w.height);
            }
            Ok(())
        }
        Cmd::ListWindows { all: false } => {
            for w in windows::list()? {
                println!("{:>6}  {}x{}  {:<24} {}", w.id, w.width, w.height, w.app_name, w.title);
            }
            Ok(())
        }
        Cmd::CaptureTest { window, seconds, out, bitrate_kbps, fps, h264 } => {
            let codec = if h264 { umac_proto::Codec::H264 } else { umac_proto::Codec::Hevc };
            capture_test::run(&window, seconds, &out, codec, bitrate_kbps, fps)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_main(_cmd: Cmd) -> anyhow::Result<()> {
    anyhow::bail!("umac-host only runs on macOS")
}
