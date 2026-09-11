//! Media-worker process spawning shared by recording, probe, live, and playback.

use std::process::{Child, Command};

/// Initializes the process-wide containment boundary before spawn, applies
/// platform background-process flags, and then creates the worker.
///
/// On Windows the desktop process joins the kill-on-close Job Object first, so
/// ordinary children inherit containment atomically. There is deliberately no
/// post-spawn AssignProcessToJobObject call: that second assignment was
/// redundant and introduced a machine-dependent failure point after a healthy
/// worker had already started.
pub(crate) fn spawn_worker(command: &mut Command) -> std::io::Result<Child> {
    nian_platform_windows::initialize_worker_process_containment()?;
    nian_platform_windows::configure_worker_command(command);
    command.spawn()
}
