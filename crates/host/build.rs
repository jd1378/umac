use std::path::Path;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    swift_library_path();
    pyrowave();
}

// The screencapturekit crate only adds the Xcode.app Swift library path. With Command Line
// Tools alone the Swift compatibility libraries live elsewhere, so add that path too.
fn swift_library_path() {
    let Ok(out) = std::process::Command::new("xcode-select").arg("-p").output() else { return };
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let lib = Path::new(&dev).join("usr/lib/swift/macosx");
    if lib.exists() {
        println!("cargo:rustc-link-search=native={}", lib.display());
    }
}

// The vendored PyroWave Metal encoder (third_party/pyrowave-metal, see its README).
fn pyrowave() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/pyrowave-metal");
    let sources = ["pyrowave_common.mm", "pyrowave_encoder.mm", "pyrowave_bitstream.cpp"];
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").include(&dir).flag("-fobjc-arc").flag("-fvisibility=hidden").warnings(false);
    for s in sources {
        build.file(dir.join(s));
        println!("cargo:rerun-if-changed={}", dir.join(s).display());
    }
    for h in ["pyrowave_metal.h", "pyrowave_common.hpp", "pyrowave_bitstream.hpp", "shaders/pyrowave_msl.h"] {
        println!("cargo:rerun-if-changed={}", dir.join(h).display());
    }
    build.compile("pyrowave_metal");
    for fw in ["Metal", "Foundation", "IOSurface", "CoreVideo"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
    println!("cargo:rustc-link-lib=c++");
}
