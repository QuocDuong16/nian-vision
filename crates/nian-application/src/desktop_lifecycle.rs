//! Rust-authoritative desktop lifecycle admission state.

use std::sync::Mutex;

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopLifecycleState {
    Running,
    Suspending,
    Quitting,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DesktopLifecycleError {
    #[error("desktop application is quitting")]
    Quitting,
    #[error("desktop application is suspending")]
    Suspending,
    #[error("desktop lifecycle state is unavailable")]
    Synchronization,
}

#[derive(Debug)]
pub struct DesktopLifecycle {
    state: Mutex<DesktopLifecycleState>,
}

impl Default for DesktopLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopLifecycle {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(DesktopLifecycleState::Running),
        }
    }

    pub fn state(&self) -> Result<DesktopLifecycleState, DesktopLifecycleError> {
        self.state
            .lock()
            .map(|state| *state)
            .map_err(|_| DesktopLifecycleError::Synchronization)
    }

    pub fn require_running(&self) -> Result<(), DesktopLifecycleError> {
        match self.state()? {
            DesktopLifecycleState::Running => Ok(()),
            DesktopLifecycleState::Suspending => Err(DesktopLifecycleError::Suspending),
            DesktopLifecycleState::Quitting => Err(DesktopLifecycleError::Quitting),
        }
    }

    /// Returns true only for the caller that won the transition to Quitting.
    pub fn begin_quit(&self) -> Result<bool, DesktopLifecycleError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DesktopLifecycleError::Synchronization)?;
        if *state == DesktopLifecycleState::Quitting {
            return Ok(false);
        }
        *state = DesktopLifecycleState::Quitting;
        Ok(true)
    }

    /// Enters the short power-transition admission state. The caller must not
    /// perform slow teardown while holding a Windows power callback.
    pub fn begin_suspend(&self) -> Result<bool, DesktopLifecycleError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DesktopLifecycleError::Synchronization)?;
        match *state {
            DesktopLifecycleState::Running => {
                *state = DesktopLifecycleState::Suspending;
                Ok(true)
            }
            DesktopLifecycleState::Suspending => Ok(false),
            DesktopLifecycleState::Quitting => Err(DesktopLifecycleError::Quitting),
        }
    }

    pub fn resume(&self) -> Result<bool, DesktopLifecycleError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DesktopLifecycleError::Synchronization)?;
        match *state {
            DesktopLifecycleState::Running => Ok(false),
            DesktopLifecycleState::Suspending => {
                *state = DesktopLifecycleState::Running;
                Ok(true)
            }
            DesktopLifecycleState::Quitting => Err(DesktopLifecycleError::Quitting),
        }
    }

    pub fn activation_allowed(&self) -> bool {
        self.state()
            .is_ok_and(|state| state == DesktopLifecycleState::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_is_idempotent_and_wins_over_resume() {
        let lifecycle = DesktopLifecycle::new();
        assert!(lifecycle.begin_quit().unwrap());
        assert!(!lifecycle.begin_quit().unwrap());
        assert_eq!(lifecycle.resume(), Err(DesktopLifecycleError::Quitting));
        assert_eq!(
            lifecycle.require_running(),
            Err(DesktopLifecycleError::Quitting)
        );
        assert!(!lifecycle.activation_allowed());
    }

    #[test]
    fn suspend_blocks_new_work_until_resume() {
        let lifecycle = DesktopLifecycle::new();
        assert!(lifecycle.begin_suspend().unwrap());
        assert_eq!(
            lifecycle.require_running(),
            Err(DesktopLifecycleError::Suspending)
        );
        assert!(lifecycle.resume().unwrap());
        assert_eq!(lifecycle.require_running(), Ok(()));
    }
}
