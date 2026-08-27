//! Application-level error model.

/// Errors surfaced by the application layer.
///
/// [`ApplicationError::ui_message`] maps each variant to a short, human-safe
/// sentence for display; technical detail stays in `Display`/logs (master
/// spec §16). Details arriving in variant payloads MUST already be
/// secret-free (the worker protocol enforces this upstream).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplicationError {
    /// Configuration failed validation.
    #[error("configuration error: {0}")]
    ConfigValidation(String),

    /// Spawning the media worker process failed.
    #[error("worker launch error: {0}")]
    WorkerLaunch(String),

    /// The worker violated its IPC contract (bad handshake, refused call,
    /// malformed frame) or died mid-session.
    #[error("worker protocol error: {0}")]
    WorkerProtocol(String),

    /// The worker permanently refused desired recording state (invalid
    /// configuration, unknown source kind): supervision stops instead of
    /// retrying forever (M3 §14/§8).
    #[error("permanent recording configuration failure: {0}")]
    PermanentRecordingConfig(String),
}

impl ApplicationError {
    /// Human-readable message safe to render in the UI.
    pub fn ui_message(&self) -> String {
        match self {
            Self::ConfigValidation(_) => {
                "Application settings are invalid. Please review them in Settings.".to_owned()
            }
            Self::WorkerLaunch(_) | Self::WorkerProtocol(_) => {
                "The media service is unavailable. Recording will resume automatically.".to_owned()
            }
            // Permanent failures DO need operator attention — but never raw
            // technical detail in the UI.
            Self::PermanentRecordingConfig(_) => {
                "Recording cannot start with the current settings. Please review them.".to_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_message_never_contains_technical_detail() {
        let err = ApplicationError::ConfigValidation("storage_root=/dev/null is bad".to_owned());
        let ui = err.ui_message();
        assert!(!ui.contains("/dev/null"));
        assert!(ui.contains("Settings"));
    }

    #[test]
    fn worker_errors_render_as_transient_for_the_ui() {
        assert!(
            ApplicationError::WorkerLaunch("binary missing".to_owned())
                .ui_message()
                .contains("automatically")
        );
    }

    #[test]
    fn permanent_failures_ask_the_operator_to_act() {
        let err = ApplicationError::PermanentRecordingConfig(
            "worker refused: invalid_params:missing 'camera'".to_owned(),
        );
        let ui = err.ui_message();
        assert!(
            !ui.contains("invalid_params"),
            "codes must stay out of the UI"
        );
        assert!(ui.contains("review"), "{ui}");
    }
}
