//! Desktop host process: owns the UI window and exposes Tauri commands.
//!
//! Media work never happens here; it belongs to the `nian-media-worker`
//! process (see ADR-0003). This crate stays FFmpeg-free.

#![forbid(unsafe_code)]

use serde::Serialize;
use tracing_subscriber::EnvFilter;

/// Basic information about the running desktop application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    /// Product name shown in the UI.
    pub name: String,
    /// Application version from the package metadata.
    pub version: String,
}

/// Returns static application metadata.
///
/// Real commands (camera management, worker supervision) arrive in M5+.
/// Kept private: `generate_handler!` resolves it in this module, and a
/// `pub` command re-exports a macro name that collides with itself when the
/// lib is rebuilt under `cfg(test)`.
#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: "Nian Vision".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Initializes logging and starts the Tauri runtime.
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let result = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_info])
        .run(tauri::generate_context!());

    if let Err(error) = result {
        // The GUI process has nowhere better to report a failed runtime.
        #[allow(clippy::print_stderr)] // startup failure path, no logger yet
        {
            eprintln!("nian-desktop: fatal: {error}");
        }
        std::process::exit(1);
    }
}
