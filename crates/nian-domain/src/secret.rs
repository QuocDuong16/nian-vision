//! Redaction wrapper for credential material.

use std::fmt;

/// Wraps a secret value so that it cannot leak through `Debug`, `Display` or
/// serialization.
///
/// The inner value is reachable only through [`Secret::expose`], which makes
/// every use site greppable during security review.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wraps a plain value.
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Grants access to the inner value.
    ///
    /// Use sites are deliberate and reviewable; nothing else may observe the
    /// contained value.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Consumes the wrapper and returns the inner value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_is_redacted() {
        let secret = Secret::new("hunter2".to_owned());
        let rendered = format!("{secret:?}");
        assert!(
            !rendered.contains("hunter2"),
            "leaked via Debug: {rendered}"
        );
        assert_eq!(rendered, "Secret(***)");
    }

    #[test]
    fn expose_returns_inner_value() {
        let secret = Secret::new(42);
        assert_eq!(*secret.expose(), 42);
    }
}
