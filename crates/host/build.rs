// The screencapturekit crate only adds the Xcode.app Swift library path. With Command Line
// Tools alone the Swift compatibility libraries live elsewhere, so add that path too.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Ok(out) = std::process::Command::new("xcode-select").arg("-p").output() else { return };
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let lib = std::path::Path::new(&dev).join("usr/lib/swift/macosx");
    if lib.exists() {
        println!("cargo:rustc-link-search=native={}", lib.display());
    }
}
