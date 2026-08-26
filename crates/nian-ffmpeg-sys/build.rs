//! Locates the FFmpeg shared libraries to link against.
//!
//! Discovery order:
//!
//! 1. `NIAN_FFMPEG_LIB_DIR` — directory containing `libavformat`/`avformat`
//!    import libraries (works everywhere, including Windows vcpkg layouts);
//! 2. `<repo>/.ffmpeg-lib` — unversioned symlink shims created by
//!    `scripts/setup-ffmpeg-linux.sh` on Linux machines that have FFmpeg
//!    runtime libraries but no `-dev` packages;
//! 3. `pkg-config` (`libavformat`), the standard route on machines with FFmpeg
//!    development packages installed.
//!
//! A missing FFmpeg is a fatal misconfiguration; a build script's only
//! channel for that is a panic, which cargo surfaces verbatim to the user.
#![allow(clippy::expect_used, clippy::panic)]
//! Linking is always dynamic; FFmpeg is never statically linked (ADR-0002).

use std::path::{Path, PathBuf};
use std::process::Command;

const LIBS: &[&str] = &["avformat", "avcodec", "avutil"];

fn main() {
    println!("cargo:rerun-if-env-changed=NIAN_FFMPEG_LIB_DIR");

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));

    let resolved = resolve_link_config(&manifest_dir).unwrap_or_else(|message| {
        panic!(
            "cannot locate FFmpeg libraries: {message}\n\n\
             Options:\n\
             1. set NIAN_FFMPEG_LIB_DIR to a directory containing libavformat/avformat import libraries\n\
             2. on Linux without -dev packages, run scripts/setup-ffmpeg-linux.sh\n\
             3. install FFmpeg development packages (e.g. libavformat-dev) so pkg-config can find them\n\
             See docs/ffmpeg.md for details."
        )
    });

    if let Some(search_dir) = resolved.search_dir {
        println!("cargo:rustc-link-search=native={}", search_dir.display());
    }
    for lib in LIBS {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}

struct LinkConfig {
    search_dir: Option<PathBuf>,
}

fn resolve_link_config(manifest_dir: &Path) -> Result<LinkConfig, String> {
    if let Ok(dir) = std::env::var("NIAN_FFMPEG_LIB_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.is_dir() {
            return Err(format!(
                "NIAN_FFMPEG_LIB_DIR points to {}, which is not a directory",
                dir.display()
            ));
        }
        verify_dir_contains_libs(&dir)?;
        return Ok(LinkConfig {
            search_dir: Some(dir),
        });
    }

    let repo_local = manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join(".ffmpeg-lib"))
        .filter(|dir| dir.is_dir());
    if let Some(dir) = repo_local {
        verify_dir_contains_libs(&dir)?;
        return Ok(LinkConfig {
            search_dir: Some(dir),
        });
    }

    if pkg_config_has_libs() {
        // pkg-config already emits the correct -L/-l flags? No: we must emit
        // them ourselves from its output to stay dependency-free.
        return Ok(LinkConfig { search_dir: None });
    }

    Err(
        "no NIAN_FFMPEG_LIB_DIR, no .ffmpeg-lib directory, and pkg-config has no libavformat"
            .to_owned(),
    )
}

fn verify_dir_contains_libs(dir: &Path) -> Result<(), String> {
    for lib in LIBS {
        let found = [
            format!("lib{lib}.so"),
            format!("lib{lib}.dylib"),
            format!("{lib}.lib"),
        ]
        .iter()
        .any(|name| dir.join(name).is_file());
        if !found {
            let listing = std::fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.file_name().to_string_lossy().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            return Err(format!(
                "directory {} does not contain a link library for {lib} (contents: {listing})",
                dir.display()
            ));
        }
    }
    Ok(())
}

fn pkg_config_has_libs() -> bool {
    let output = Command::new("pkg-config")
        .args(["--libs", "libavformat", "libavcodec", "libavutil"])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let flags = String::from_utf8_lossy(&output.stdout);
    for flag in flags.split_whitespace() {
        if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else if let Some(lib) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib=dylib={lib}");
        }
    }
    true
}
