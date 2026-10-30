//! `umac autostart`: starts the tray daemon at login through an XDG autostart entry.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

const ICON_SVG: &[u8] = include_bytes!("../assets/tray.svg");

pub struct Paths {
    pub entry: PathBuf,
    pub icon: PathBuf,
}

impl Paths {
    pub fn for_user() -> Result<Self> {
        let config = dirs::config_dir().context("no config directory")?;
        let data = dirs::data_dir().context("no data directory")?;
        Ok(Self {
            entry: config.join("autostart/umac.desktop"),
            icon: data.join("icons/hicolor/scalable/apps/umac.svg"),
        })
    }
}

/// Quotes an Exec argument per the Desktop Entry spec when it needs it.
fn exec_arg(arg: &str) -> String {
    const RESERVED: &[char] = &[' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')', '`'];
    if !arg.contains(RESERVED) {
        return arg.replace('%', "%%");
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn entry(exe: &Path, icon: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=umac\nComment=Mac windows on this PC\nExec={} daemon\nIcon={}\n\
         Terminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\nX-KDE-autostart-phase=2\n",
        exec_arg(&exe.to_string_lossy()),
        icon.display()
    )
}

/// Writes the entry for `exe` and installs the icon. Safe to repeat.
pub fn enable(paths: &Paths, exe: &Path) -> Result<()> {
    for p in [&paths.entry, &paths.icon] {
        fs::create_dir_all(p.parent().context("path has no parent")?)?;
    }
    fs::write(&paths.icon, ICON_SVG).with_context(|| format!("writing {}", paths.icon.display()))?;
    fs::write(&paths.entry, entry(exe, &paths.icon)).with_context(|| format!("writing {}", paths.entry.display()))?;
    Ok(())
}

/// Removes the entry; returns false if it was not there.
pub fn disable(paths: &Paths) -> Result<bool> {
    match fs::remove_file(&paths.entry) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Disabled,
    /// Enabled; `current` is false when the entry starts a different binary than this one.
    Enabled { exec: String, current: bool },
}

pub fn status(paths: &Paths, exe: &Path) -> Result<Status> {
    let text = match fs::read_to_string(&paths.entry) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Status::Disabled),
        Err(e) => return Err(e.into()),
    };
    let exec = text.lines().find_map(|l| l.strip_prefix("Exec=")).unwrap_or_default().to_owned();
    let current = exec == format!("{} daemon", exec_arg(&exe.to_string_lossy()));
    Ok(Status::Enabled { exec, current })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_content_and_quoting() {
        let e = entry(Path::new("/usr/local/bin/umac"), Path::new("/i/umac.svg"));
        assert!(e.starts_with("[Desktop Entry]\n"));
        assert!(e.contains("\nExec=/usr/local/bin/umac daemon\n"));
        assert!(e.contains("\nIcon=/i/umac.svg\n"));
        assert_eq!(exec_arg("/a b/$x\"y"), "\"/a b/\\$x\\\"y\"");
        assert_eq!(exec_arg("/a/100%"), "/a/100%%");
    }

    #[test]
    fn enable_status_disable_roundtrip() {
        let dir = std::env::temp_dir().join(format!("umac-autostart-{}", std::process::id()));
        let paths = Paths { entry: dir.join("autostart/umac.desktop"), icon: dir.join("icons/umac.svg") };
        let exe = Path::new("/opt/umac/umac");
        assert_eq!(status(&paths, exe).unwrap(), Status::Disabled);
        enable(&paths, exe).unwrap();
        enable(&paths, exe).unwrap();
        assert_eq!(status(&paths, exe).unwrap(), Status::Enabled { exec: "/opt/umac/umac daemon".into(), current: true });
        assert!(matches!(status(&paths, Path::new("/elsewhere/umac")).unwrap(), Status::Enabled { current: false, .. }));
        assert!(paths.icon.exists());
        assert!(disable(&paths).unwrap());
        assert!(!disable(&paths).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
}
