// Prevents an extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Some(code) = nian_desktop::notification_helper_exit_code() {
        std::process::exit(code);
    }
    nian_desktop::run();
}
