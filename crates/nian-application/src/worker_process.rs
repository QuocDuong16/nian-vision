//! Media-worker process containment shared by recording, probe, and playback.

use std::process::Child;

/// Applies the platform hard-termination containment contract immediately
/// after spawn. Failure is fail-closed: an uncontained worker is killed and
/// reaped before the error is returned to its caller.
pub(crate) fn contain_spawned_worker(mut child: Child) -> std::io::Result<Child> {
    if let Err(error) = nian_platform_windows::contain_worker_process(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}
