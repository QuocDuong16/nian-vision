//! Application-level error model.

/// Errors surfaced by the application layer.
///
/// [`ApplicationError::ui_message`] maps each variant to a short, human-safe
/// sentence for display; technical detail stays in `Display`/logs (master
/// spec §16).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplicationError {
    /// Configuration failed validation.
    #[error("configuration error: {0}")]
    ConfigValidation(String),
}

impl ApplicationError {
    /// Human-readable message safe to render in the UI.
    pub fn ui_message(&self) -> String {
        match self {
            Self::ConfigValidation(_) => {
                "Application settings are invalid. Please review them in Settings.".to_owned()
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
}
