fn main() {
    // Linux release staging keeps the FFmpeg SONAME libraries under
    // lib/nian-vision, so the pre-bundle worker uses a relative RUNPATH and can
    // be smoke-tested without LD_LIBRARY_PATH. Tauri's AppImage bundler later
    // normalizes executable RUNPATH to $ORIGIN/../lib; the release config maps
    // the same application-owned FFmpeg libraries into the AppImage-private
    // /usr/lib directory to match that final loader contract.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/nian-vision");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
