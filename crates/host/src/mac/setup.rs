//! `umac-host setup`: install, sign, register the LaunchAgent, walk the user through
//! permissions and print a pairing string. Safe to re-run.

use std::{
    io::{BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use umac_transport::{
    Identity,
    identity::{create_private_dir, random_token, write_private},
};

use super::permissions::{ACCESSIBILITY_PANE, SCREEN_RECORDING_PANE};
use crate::{auth::AuthStore, config, status::Status};

const SIGNING_NAME: &str = "umac host signing";

fn step(n: u8, text: &str) {
    println!("\n[{n}] {text}");
}

fn run(cmd: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ask(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    matches!(line.trim(), "y" | "Y" | "yes")
}

fn wait_enter(prompt: &str) {
    if std::io::stdin().is_terminal() {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let _ = std::io::stdin().lock().read_line(&mut String::new());
    }
}

fn uid() -> String {
    run("id", &["-u"]).map(|s| s.trim().to_owned()).unwrap_or_default()
}

fn console_user() -> String {
    run("stat", &["-f", "%Su", "/dev/console"]).map(|s| s.trim().to_owned()).unwrap_or_default()
}

pub fn setup() -> anyhow::Result<()> {
    println!("umac-host setup: makes this Mac stream its windows to your other computers.");

    step(1, "Checking user session");
    let me = std::env::var("USER").unwrap_or_default();
    let console = console_user();
    if me == console {
        println!("    ok: {me} owns the desktop session");
    } else if run("launchctl", &["print", &format!("gui/{}", uid())]).is_ok() {
        // Fast user switching: the agent runs in the background and streams once {me} is on screen.
        println!("    {me} is logged in but {console} is on the screen. Installing anyway: umac streams {me}'s");
        println!("    windows while {me} is on screen. Grant permissions then (re-run setup as {me}).");
    } else {
        bail!(
            "{me} is not logged in on the Mac. Log in as {me} (at the Mac or over Screen Sharing; fast\n\
             user switching keeps others logged in), then run setup again."
        );
    }

    step(2, "Installing the agent binary");
    let installed = install_binary()?;
    println!("    installed {}", installed.display());
    match sign(&installed) {
        Ok(()) => println!("    signed with a local identity, so permissions survive updates"),
        Err(e) => println!(
            "    warning: could not sign ({e:#}).\n    Permissions will have to be granted again after every update."
        ),
    }

    step(3, "Creating host identity");
    let id = Identity::load_or_create(&config::identity_dir())?;
    println!("    fingerprint {}", id.fingerprint());

    step(4, "Registering the background agent (LaunchAgent)");
    let before = Status::read().map(|s| s.pid);
    install_launch_agent(&installed)?;
    let status = wait_for_agent(before)?;
    println!("    agent running (pid {})", status.pid);
    if let Some(e) = &status.error {
        println!("    agent reports: {e}");
    }

    step(5, "Checking permissions");
    ensure_permissions()?;

    step(6, "Checking power and login settings");
    if me == console {
        check_auto_login();
        check_sleep();
        check_display();
    } else {
        println!("    skipped: checked when {me} is on screen");
    }

    step(7, "Pairing");
    print_pairing()?;
    println!("\nSetup complete. Re-run `umac-host setup` any time; it is safe to repeat.");
    Ok(())
}

fn install_binary() -> anyhow::Result<PathBuf> {
    let src = std::env::current_exe()?.canonicalize()?;
    let dst = config::install_path();
    create_private_dir(dst.parent().unwrap())?;
    if src != dst.canonicalize().unwrap_or_default() {
        // Copy then rename, so a running agent keeps its old binary until restarted.
        let tmp = dst.with_extension("new");
        std::fs::copy(&src, &tmp)?;
        std::fs::rename(&tmp, &dst)?;
    }
    Ok(dst)
}

struct SigningKeychain {
    path: PathBuf,
    password: String,
}

fn signing_keychain() -> anyhow::Result<SigningKeychain> {
    let dir = config::support_dir().join("signing");
    create_private_dir(&dir)?;
    let path = dir.join("umac-signing.keychain-db");
    let pw_path = dir.join("keychain-password");
    if path.exists() && pw_path.exists() {
        return Ok(SigningKeychain { path, password: std::fs::read_to_string(pw_path)?.trim().to_owned() });
    }
    // Dedicated keychain with a generated password: usable over ssh, where the login keychain
    // stays locked, and it never touches the user's own keychain password.
    let _ = std::fs::remove_file(&path);
    let password = random_token();
    write_private(&pw_path, password.as_bytes())?;
    let p = path.to_str().ok_or_else(|| anyhow!("non UTF-8 path"))?;
    run("security", &["create-keychain", "-p", &password, p])?;
    run("security", &["set-keychain-settings", p])?;
    run("security", &["unlock-keychain", "-p", &password, p])?;

    let tmp = dir.join("tmp");
    create_private_dir(&tmp)?;
    let cnf = tmp.join("cert.cnf");
    std::fs::write(
        &cnf,
        format!(
            "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN={SIGNING_NAME}\n\
             [ext]\nbasicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=critical,codeSigning\n"
        ),
    )?;
    let s = |p: &Path| p.to_str().unwrap().to_owned();
    let (key, cert, p12) = (tmp.join("key.pem"), tmp.join("cert.pem"), tmp.join("id.p12"));
    let p12_pw = random_token();
    let res = (|| -> anyhow::Result<()> {
        run("openssl", &["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650", "-keyout", &s(&key), "-out", &s(&cert), "-config", &s(&cnf)])?;
        run("openssl", &["pkcs12", "-export", "-inkey", &s(&key), "-in", &s(&cert), "-out", &s(&p12), "-passout", &format!("pass:{p12_pw}"), "-name", SIGNING_NAME])?;
        run("security", &["import", &s(&p12), "-k", p, "-P", &p12_pw, "-T", "/usr/bin/codesign"])?;
        run("security", &["set-key-partition-list", "-S", "apple-tool:,apple:", "-s", "-k", &password, p])?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    res?;
    Ok(SigningKeychain { path, password })
}

/// Signs with a stable self-signed certificate. TCC records the code signing requirement
/// (identifier + certificate), so grants survive rebuilds; ad-hoc signatures change on
/// every build and silently lose Screen Recording and Accessibility.
fn sign(binary: &Path) -> anyhow::Result<()> {
    let kc = signing_keychain()?;
    let p = kc.path.to_str().unwrap();
    run("security", &["unlock-keychain", "-p", &kc.password, p])?;
    let ids = run("security", &["find-identity", "-p", "codesigning", p])?;
    let hash = ids
        .lines()
        .find(|l| l.contains(SIGNING_NAME))
        .and_then(|l| l.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("signing identity missing from {p}"))?
        .to_owned();
    run("codesign", &["-f", "-s", &hash, "--keychain", p, "-i", config::LABEL, binary.to_str().unwrap()])?;
    Ok(())
}

fn plist(binary: &Path) -> String {
    let logs = config::log_dir();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array><string>{bin}</string><string>run</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>LimitLoadToSessionType</key><string>Aqua</string>
    <key>ProcessType</key><string>Interactive</string>
    <key>ThrottleInterval</key><integer>5</integer>
    <key>StandardOutPath</key><string>{out}</string>
    <key>StandardErrorPath</key><string>{out}</string>
</dict>
</plist>
"#,
        label = config::LABEL,
        bin = binary.display(),
        out = logs.join("agent.log").display(),
    )
}

fn install_launch_agent(binary: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(config::log_dir())?;
    let path = config::launch_agent_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, plist(binary))?;
    let domain = format!("gui/{}", uid());
    let service = format!("{domain}/{}", config::LABEL);
    // Restart unconditionally so a freshly installed binary is the one running.
    let _ = run("launchctl", &["bootout", &service]);
    std::thread::sleep(Duration::from_millis(500));
    // With our agent stopped, a port in use belongs to someone else (often another Mac user's
    // agent), so the port is chosen now.
    pick_port()?;
    run("launchctl", &["bootstrap", &domain, path.to_str().unwrap()]).context("launchctl bootstrap")?;
    Ok(())
}

fn port_free(port: u16) -> bool {
    std::net::UdpSocket::bind((std::net::Ipv6Addr::UNSPECIFIED, port)).is_ok()
}

/// Keeps the saved port while it is free, else takes this user's own one or the next free
/// one, and saves it so the agent and pairing links agree.
fn pick_port() -> anyhow::Result<u16> {
    let saved = config::Settings::saved_port();
    let uid: u32 = uid().parse().context("reading the user id")?;
    let port = config::choose_port(saved, uid, port_free)
        .ok_or_else(|| anyhow!("no free UDP port near {}; set \"port\" in {}", config::preferred_port(uid), config::settings_path().display()))?;
    if saved != Some(port) {
        config::Settings::save_port(port).context("saving the port")?;
    }
    match saved {
        Some(old) if old != port => println!(
            "    port {old} is taken (another user's agent?); now using {port}.\n    Clients paired before must pair again."
        ),
        _ => println!("    listening on UDP port {port}"),
    }
    Ok(port)
}

fn restart_agent() -> anyhow::Result<()> {
    run("launchctl", &["kickstart", "-k", &format!("gui/{}/{}", uid(), config::LABEL)])?;
    Ok(())
}

fn wait_for_agent(previous_pid: Option<u32>) -> anyhow::Result<Status> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(s) = Status::read().filter(|s| Some(s.pid) != previous_pid && pid_alive(s.pid)) {
            return Ok(s);
        }
        if Instant::now() > deadline {
            bail!("agent did not start; see {}", config::log_dir().join("agent.log").display());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn ensure_permissions() -> anyhow::Result<()> {
    for attempt in 0.. {
        let s = Status::read().ok_or_else(|| anyhow!("agent status missing"))?;
        if s.screen_recording && s.accessibility {
            println!("    ok: Screen Recording and Accessibility are allowed");
            return Ok(());
        }
        if attempt == 0 {
            // Opening the panes from here works because we run as the desktop user.
            for (granted, pane) in [(s.screen_recording, SCREEN_RECORDING_PANE), (s.accessibility, ACCESSIBILITY_PANE)] {
                if !granted {
                    let _ = run("open", &[pane]);
                }
            }
        }
        println!("    Missing permissions. Connect to this Mac with Screen Sharing, then in");
        println!("    System Settings > Privacy & Security:");
        if !s.screen_recording {
            println!("      - Screen & System Audio Recording: turn ON \"umac-host\"");
            println!("        (if macOS offers \"Quit & Reopen\", choose \"Later\"; setup restarts the agent)");
        }
        if !s.accessibility {
            println!("      - Accessibility: turn ON \"umac-host\"");
        }
        println!("    If umac-host is not listed, click + and add {}", config::install_path().display());
        if !std::io::stdin().is_terminal() {
            println!("    (not interactive: re-run setup after granting)");
            return Ok(());
        }
        wait_enter("    Press Enter after granting... ");
        // Screen Recording only takes effect in a new process.
        let pid = s.pid;
        restart_agent()?;
        wait_for_agent(Some(pid))?;
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

fn check_auto_login() {
    let user = run("defaults", &["read", "/Library/Preferences/com.apple.loginwindow", "autoLoginUser"]).unwrap_or_default();
    let me = std::env::var("USER").unwrap_or_default();
    if user.trim() == me {
        println!("    ok: automatic login is enabled for {me}");
    } else {
        println!("    Automatic login is off. After a reboot nobody is logged in and nothing can be streamed.");
        println!("    Enable it in System Settings > Users & Groups > Automatically log in as: {me}");
        println!("    (requires FileVault to be off).");
    }
}

fn check_sleep() {
    let pm = run("pmset", &["-g"]).unwrap_or_default();
    let value = |key: &str| {
        pm.lines()
            .map(str::trim)
            .find(|l| l.split_whitespace().next() == Some(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u32>().ok())
    };
    let (sleep, display) = (value("sleep"), value("displaysleep"));
    if sleep == Some(0) && display == Some(0) {
        println!("    ok: system and display sleep are disabled");
        return;
    }
    println!("    System sleep = {sleep:?}, display sleep = {display:?} minutes.");
    println!("    A sleeping display stops window capture. Recommended:");
    let cmd = ["pmset", "-a", "sleep", "0", "displaysleep", "0"];
    println!("      sudo {}", cmd.join(" "));
    if ask("    Run it now (sudo will ask for your password)?") {
        match Command::new("sudo").args(cmd).status() {
            Ok(s) if s.success() => println!("    done"),
            _ => println!("    failed; run the command manually"),
        }
    }
}

fn check_display() {
    let info = run("system_profiler", &["SPDisplaysDataType"]).unwrap_or_default();
    let displays = info.lines().filter(|l| l.trim_start().starts_with("Resolution:")).count();
    if displays == 0 {
        println!("    warning: no display is attached. Windows have nowhere to live; attach an HDMI dummy plug.");
    } else {
        println!("    {displays} display(s) attached. Without a monitor or HDMI dummy plug macOS may fall back to a");
        println!("    low resolution after reboot; a 4K dummy plug gives windows room to grow.");
    }
}

pub fn lan_ip() -> Option<std::net::IpAddr> {
    // Connecting a UDP socket sends nothing; it only selects the outgoing interface.
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip())
}

pub fn print_pairing() -> anyhow::Result<()> {
    let settings = config::Settings::load();
    let id = Identity::load_or_create(&config::identity_dir())?;
    let token = AuthStore::new(&config::support_dir()).issue_token()?;
    let ip = lan_ip().map(|i| i.to_string()).unwrap_or_else(|| "<mac-ip>".into());
    println!("    On your PC run (valid 10 minutes, single use):\n");
    println!("      umac pair 'umac://{ip}:{}?fp={}&token={token}'", settings.port, id.fingerprint());
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    let id = Identity::load_or_create(&config::identity_dir())?;
    println!("host fingerprint: {}", id.fingerprint());
    match Status::read() {
        Some(s) => {
            let alive = pid_alive(s.pid);
            println!("agent: pid {} ({}) version {}", s.pid, if alive { "running" } else { "not running" }, s.version);
            println!("listening: {:?}", s.listening);
            println!("screen recording: {}, accessibility: {}", s.screen_recording, s.accessibility);
            if let Some(e) = s.error {
                println!("error: {e}");
            }
        }
        None => println!("agent: never started (run `umac-host setup`)"),
    }
    let clients = AuthStore::new(&config::support_dir()).clients();
    println!("paired clients: {}", clients.len());
    for c in clients {
        println!("  {} {}", &c.fingerprint[..16], c.name);
    }
    Ok(())
}

pub fn uninstall(purge: bool) -> anyhow::Result<()> {
    let service = format!("gui/{}/{}", uid(), config::LABEL);
    let _ = run("launchctl", &["bootout", &service]);
    let _ = std::fs::remove_file(config::launch_agent_path());
    let _ = std::fs::remove_file(config::install_path());
    println!("agent stopped and removed");
    if purge {
        std::fs::remove_dir_all(config::support_dir()).ok();
        println!("removed {} (identity, paired clients, signing keychain)", config::support_dir().display());
    } else {
        println!("kept identity and paired clients in {} (use --purge to delete)", config::support_dir().display());
    }
    println!("Permission entries remain in System Settings > Privacy & Security; remove them there if wanted.");
    Ok(())
}
