fn main() {
    // Linux staging and the final AppImage keep the private FFmpeg SONAME
    // libraries under lib/nian-vision. The worker therefore uses the same
    // installation-relative RUNPATH in both places and can be validated without
    // LD_LIBRARY_PATH assistance.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/nian-vision");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
