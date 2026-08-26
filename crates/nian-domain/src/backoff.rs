//! Reconnect backoff schedule.
//!
//! The delays are a product decision (see master spec §11): failures retry at
//! 2s, 5s, 10s, 30s and then stay at 60s forever until the connection is
//! restored.

use std::time::Duration;

/// Fixed reconnect delay sequence in seconds.
pub const RECONNECT_DELAY_SECS: [u64; 5] = [2, 5, 10, 30, 60];

/// State machine producing the next reconnect delay after consecutive
/// failures.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconnectBackoff {
    attempt: u32,
}

impl ReconnectBackoff {
    /// Delay to wait before attempt number `attempt + 1`.
    ///
    /// Saturates at the final schedule entry.
    pub fn next_delay(&mut self) -> Duration {
        let index = (self.attempt as usize).min(RECONNECT_DELAY_SECS.len() - 1);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_secs(RECONNECT_DELAY_SECS[index])
    }

    /// Number of consecutive failures observed since the last reset.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// Clears the failure streak, e.g. after a successful connection.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_specified_schedule_then_saturates() {
        let mut backoff = ReconnectBackoff::default();
        let expected = [2, 5, 10, 30, 60, 60, 60, 60];
        for secs in expected {
            assert_eq!(backoff.next_delay(), Duration::from_secs(secs));
        }
        assert_eq!(backoff.attempts(), 8);
    }

    #[test]
    fn reset_restores_initial_delays() {
        let mut backoff = ReconnectBackoff::default();
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
        backoff.reset();
        assert_eq!(backoff.attempts(), 0);
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
    }
}
