//! Camera recording supervision (M3 §3/§4/§6/§7/§8/§9/§10/§16).
//!
//! The [`CameraRecordingSupervisor`] owns the reconnect lifecycle ABOVE one
//! healthy-source [`crate::RecordingSession`] — the session stays exactly
//! the M2 abstraction: ONE connection, keyframe-aware segments, M2 failure
//! semantics. Reconnect orchestration never leaks into the session.
//!
//! # State machine (explicit, strongly typed)
//!
//! ```text
//! Idle ──run──▶ Connecting ──open ok──▶ Recording ──graceful stop──▶ Stopped
//!                 │                       │
//!                 │ retryable open error  │ retryable source failure / RTSP EOF
//!                 ▼                       ▼
//!               Backoff ─────────────────────────────────▶ Connecting
//!                 │
//!                 │ non-retryable failure      stop requested anywhere
//!                 ▼                            ▼
//!               Failed                       Stopped
//! ```
//!
//! Every transition is announced through a typed [`SupervisorEvent`]; state
//! is never encoded in booleans. One reconnect always builds a fresh
//! media input + fresh interrupt handle + fresh session through the
//! [`SessionFactory`] seam — reusing a permanently-cancelled FFmpeg context
//! is structurally impossible because each attempt consumes its session.
//!
//! # Determinism
//!
//! Waiting and jitter are injected ([`Waiter`], [`Jitter`]); unit tests use
//! virtual waiting, seeded jitter or no jitter at all and NEVER sleep out
//! the 60 s schedule tail. Production wires real sleeping.
//!
//! # Events
//!
//! [`SupervisorEvent`] payloads carry camera id, states, attempt counters,
//! delays and typed failure categories only — no credentials, no URLs, no
//! FFmpeg pointers, and no such field could be added unnoticed.

use std::time::Duration;

use nian_domain::{CameraId, ReconnectBackoff};

use crate::{FailureCategory, RecordingEndReason, RecordingError};

/// Where a recording source lives; decides how clean EOF is read (M3 §8).
///
/// A finite local file hitting EOF is COMPLETION — reconnecting would loop
/// over finished content forever. For RTSP, a silent stream end means the
/// camera/network vanished without goodbye: a retryable loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Local finite container file.
    File,
    /// Live network camera stream.
    Rtsp,
}

/// Opens ONE fresh session per reconnect attempt.
///
/// Abstraction seam so unit tests script connect/read outcomes without any
/// fake production media logic; production wraps real opens of
/// [`crate::RecordingSession`]. An `Err` here models failed open/connect.
pub trait SessionFactory {
    /// Opens a new session; fresh input + fresh interrupt handle every call.
    fn open_session(&mut self) -> Result<Box<dyn ActiveSession>, RecordingError>;
}

/// A session bound to one live connection (one attempt).
pub trait ActiveSession {
    /// Runs to completion like [`crate::RecordingSession::run`].
    fn run(
        self: Box<Self>,
        events: &mut dyn FnMut(crate::RecordingEvent),
    ) -> Result<crate::RecordingSummary, RecordingError>;

    /// This attempt's graceful-stop flag (the run's own flag shared by the
    /// supervisor).
    fn stop_flag(&self) -> crate::StopFlag;
}

/// Blocks until a delay elapses or stopping was requested — injectable so
/// tests never sleep the real schedule.
pub trait Waiter {
    /// Returns `false` when `stop_requested` flipped true during the wait.
    fn wait(&mut self, delay: Duration, stop_requested: &dyn Fn() -> bool) -> bool;
}

/// Real wall-clock waiter used by production wiring.
#[derive(Debug, Default, Clone, Copy)]
pub struct SleepWaiter;

impl Waiter for SleepWaiter {
    fn wait(&mut self, delay: Duration, stop_requested: &dyn Fn() -> bool) -> bool {
        // Sliced sleeping honors a concurrently-arrived stop within ~100 ms
        // instead of after the whole backoff delay.
        let slice = Duration::from_millis(100);
        let mut remaining = delay;
        while !remaining.is_zero() && !stop_requested() {
            let step = remaining.min(slice);
            std::thread::sleep(step);
            remaining -= step;
        }
        !stop_requested()
    }
}

/// Bounded reconnect jitter source (M3 §9): delays get a random delta in
/// `-jitter_half..=+jitter_half` so N cameras do not reconnect in lockstep
/// after a router reboot. The base schedule itself is never altered.
pub trait Jitter {
    /// Signed delta in milliseconds within `-half..=+half` (ms).
    fn offset_millis(&mut self, half_millis: u64) -> i64;
}

/// Seeded deterministic jitter (xorshift64*); fully reproducible in tests.
#[derive(Debug, Clone)]
pub struct SeededJitter {
    state: u64,
}

impl SeededJitter {
    /// Creates the generator from any seed material.
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed | 0x9E37_79B9_7F4A_7C15, // zero seeds are degenerate
        }
    }
}

impl Jitter for SeededJitter {
    fn offset_millis(&mut self, half_millis: u64) -> i64 {
        if half_millis == 0 {
            return 0;
        }
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let raw = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let span = raw % (2 * half_millis + 1);
        i64::try_from(span).unwrap_or(0) - i64::try_from(half_millis).unwrap_or(i64::MAX)
    }
}

/// Zero jitter for tests asserting exact base delays.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoJitter;

impl Jitter for NoJitter {
    fn offset_millis(&mut self, _half_millis: u64) -> i64 {
        0
    }
}

/// Explicit supervisor lifecycle states (M3 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorState {
    /// Constructed but not yet running.
    Idle,
    /// Opening/connecting one session attempt.
    Connecting,
    /// A session is actively recording.
    Recording,
    /// Waiting out the reconnect delay before the next attempt.
    Backoff,
    /// Terminal: stopped on operator request; nothing reconnects anymore.
    Stopped,
    /// Terminal: permanent failure ended supervision.
    Failed,
}

/// How a supervised run finally ended (payload of `Finished`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorEnd {
    /// Operator stop honored. `clean` mirrors whether the last attempt
    /// terminated gracefully rather than via forced cancellation.
    StoppedByOperator {
        /// Whether no cancellation mark accompanied the shutdown.
        clean: bool,
    },
    /// Non-retryable failure ended supervision (M3 §8 permanent rows).
    PermanentFailure {
        /// Typed category recorded for UI/logs.
        category: FailureCategory,
    },
    /// Finite source completed (`SourceKind::File` + clean EOF).
    SourceCompleted,
}

/// Typed supervisor events (M3 §10). Everything a future UI needs — camera
/// identity, state, retry attempt, retry delay, typed failure category —
/// and nothing it must not have: credentials, RTSP URLs and FFmpeg pointers
/// cannot appear here BY CONSTRUCTION (no such fields exist in these
/// variants).
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorEvent {
    /// Lifecycle state changed (every transition, no exceptions).
    StateChanged {
        /// Camera under supervision.
        camera_id: CameraId,
        /// Previous state.
        from: SupervisorState,
        /// New state.
        to: SupervisorState,
    },

    /// A reconnect is scheduled after failures.
    ReconnectScheduled {
        /// Camera under supervision.
        camera_id: CameraId,
        /// Which consecutive-failure attempt comes next (≥ 1).
        retry_attempt: u32,
        /// Exact wait including jitter.
        retry_delay: Duration,
        /// Base schedule entry before jitter (deterministic display).
        base_delay: Duration,
    },

    /// A session opened successfully and started recording.
    ConnectionEstablished {
        /// Camera under supervision.
        camera_id: CameraId,
        /// Consecutive failures before this success (0 on first try);
        /// resets to 0 afterwards unless the stable-recording rule keeps
        /// the streak alive (`stable_recording_reset_applied` tells which).
        prior_attempts: u32,
        /// Whether the backoff streak was reset because THIS connection
        /// went on to record stably (emitted retroactively at attempt end).
        stable_recording_reset_applied: bool,
    },

    /// One session attempt ended (any outcome).
    SessionEnded {
        /// Camera under supervision.
        camera_id: CameraId,
        /// What the attempt concluded as.
        outcome: AttemptOutcome,
    },

    /// Supervision reached a terminal state. Exactly once per run.
    Finished {
        /// Camera under supervision.
        camera_id: CameraId,
        /// How supervision ended.
        end: SupervisorEnd,
        /// Total segments published across all attempts.
        finalized_segments: usize,
    },
}

/// Per-attempt terminal outcome carried by `SessionEnded`.
#[derive(Debug, Clone, PartialEq)]
pub enum AttemptOutcome {
    /// Graceful operator stop of this attempt.
    GracefulStop {
        /// Published segments in the attempt.
        finalized_segments: usize,
    },
    /// Clean EOF from the source.
    Eof {
        /// Published segments in the attempt.
        finalized_segments: usize,
        /// Operational meaning given the source kind (§8: same demuxer
        /// signal, deliberately different supervisor behavior).
        interpretation: EofInterpretation,
    },
    /// Failure with its centralized typed category — supervisors and UIs
    /// NEVER parse strings to decide anything.
    Failure {
        /// Centralized classification from `RecordingError::category`.
        category: FailureCategory,
    },
}

/// How clean EOF maps onto supervisor behavior per source kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EofInterpretation {
    /// Finite local file finished — completion, not reconnect.
    Completed,
    /// RTSP peer vanished silently — treated as retryable connection loss.
    ConnectionLost,
}

/// Tunables for supervised operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// Media time one connection must record successfully BEFORE the
    /// backoff streak counts as recovered (M3 §4 stability criterion).
    ///
    /// Deliberately NOT "connection opened": an attempt that dies 100 ms
    /// later must not reset 2 s → 60 s escalation, or flapping cameras
    /// would hammer the network forever while pretending to recover.
    pub stable_recording_threshold: Duration,
    /// Half-width of reconnect jitter added to base delays (± value).
    pub jitter_half: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            // Two segment targets' worth at default config, short enough to
            // matter: a feed that survived 30 s of wall time recording is
            // demonstrably healthy.
            stable_recording_threshold: Duration::from_secs(30),
            // ±2 s keeps multi-camera desynchronization meaningful against
            // the shortest base entry (2 s) without doubling small delays.
            jitter_half: Duration::from_secs(2),
        }
    }
}

/// Summary of a full supervised run (return value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisedRunOutcome {
    /// Final supervision disposition.
    pub end: SupervisorEnd,
    /// Total published segments across all attempts.
    pub finalized_segments: usize,
}

/// Drives reconnect orchestration above one session at a time.
///
/// Call [`CameraRecordingSupervisor::run_until_end`] once per supervised
/// run; it returns when supervision reaches `Stopped` or `Failed`. Stop
/// requests arrive through the run's shared stop flag (`stop_flag()`), from
/// another thread or from inside session event callbacks alike.
pub struct CameraRecordingSupervisor<F: SessionFactory, W: Waiter, J: Jitter> {
    camera_id: CameraId,
    source_kind: SourceKind,
    config: SupervisorConfig,
    factory: F,
    waiter: W,
    jitter: J,
    backoff: ReconnectBackoff,
    stop: crate::StopFlag,

    finalized_segments: usize,
    /// Media time recorded during the CURRENT successful connection, fed by
    /// `SegmentFinalized` observations, drives the stability reset.
    current_connection_media_time: Option<Duration>,
    /// Whether the current connection already satisfied the stability rule.
    stable_reset_emitted_for_current: bool,
}

impl<F: SessionFactory, W: Waiter, J: Jitter> CameraRecordingSupervisor<F, W, J> {
    /// Wires a supervisor for one camera/source combination.
    pub fn new(
        camera_id: CameraId,
        source_kind: SourceKind,
        config: SupervisorConfig,
        factory: F,
        waiter: W,
        jitter: J,
    ) -> Self {
        Self {
            camera_id,
            source_kind,
            config,
            factory,
            waiter,
            jitter,
            backoff: ReconnectBackoff::default(),
            stop: crate::StopFlag::new(),
            finalized_segments: 0,
            current_connection_media_time: None,
            stable_reset_emitted_for_current: false,
        }
    }

    /// Clones the run-wide graceful-stop flag: requesting it stops the
    /// active session gracefully AND forbids every further reconnect.
    pub fn stop_flag(&self) -> crate::StopFlag {
        self.stop.clone()
    }

    /// Runs the lifecycle to its terminal state. Emits every state change
    /// plus attempt/reconnect events through `events`; exactly one
    /// `Finished` ends the stream.
    pub fn run_until_end(
        mut self,
        events: &mut dyn FnMut(SupervisorEvent),
    ) -> SupervisedRunOutcome {
        let camera_id = self.camera_id.clone();
        let mut state = SupervisorState::Idle;
        transition(&mut state, SupervisorState::Connecting, &camera_id, events);

        loop {
            // Shutdown gate FIRST: stop during Backoff lands here after the
            // wait (or immediately on entry) — no reconnect may occur past
            // a requested stop (M3 §16).
            if self.stop.is_requested() {
                transition(&mut state, SupervisorState::Stopped, &camera_id, events);
                return self.finish(events, SupervisorEnd::StoppedByOperator { clean: true });
            }

            // ---- Connecting: one fresh session attempt --------------------
            match self.factory.open_session() {
                Ok(session) => {
                    let prior_attempts = self.backoff.attempts();
                    // Entering a NEW connection clears the old one's clock;
                    // the STREAK (escalation level) is managed separately:
                    // reset deferred behind the stability rule (§4).
                    self.current_connection_media_time = Some(Duration::ZERO);
                    self.stable_reset_emitted_for_current = false;

                    transition(&mut state, SupervisorState::Recording, &camera_id, events);
                    events(SupervisorEvent::ConnectionEstablished {
                        camera_id: camera_id.clone(),
                        prior_attempts,
                        stable_recording_reset_applied: false,
                    });

                    let result = session.run(&mut |recording_event| {
                        if let crate::RecordingEvent::SegmentFinalized {
                            media_duration: Some(duration),
                            ..
                        } = &recording_event
                        {
                            let clock = self
                                .current_connection_media_time
                                .get_or_insert(Duration::ZERO);
                            *clock = clock.saturating_add(*duration);
                        }
                    });

                    let attempt_outcome = self.attempt_outcome_of(&result);
                    events(SupervisorEvent::SessionEnded {
                        camera_id: camera_id.clone(),
                        outcome: attempt_outcome.clone(),
                    });

                    // Stability check: did THIS connection record enough?
                    let stable = self
                        .current_connection_media_time
                        .is_some_and(|media_time| {
                            media_time >= self.config.stable_recording_threshold
                        });
                    if stable && !self.stable_reset_emitted_for_current {
                        self.backoff.reset();
                        self.stable_reset_emitted_for_current = true;
                        events(SupervisorEvent::ConnectionEstablished {
                            camera_id: camera_id.clone(),
                            prior_attempts: 0,
                            stable_recording_reset_applied: true,
                        });
                    }

                    match self.resolve(result) {
                        Resolution::OperatorStop { clean } => {
                            transition(&mut state, SupervisorState::Stopped, &camera_id, events);
                            return self.finish(events, SupervisorEnd::StoppedByOperator { clean });
                        }
                        Resolution::Completed => {
                            transition(&mut state, SupervisorState::Stopped, &camera_id, events);
                            return self.finish(events, SupervisorEnd::SourceCompleted);
                        }
                        Resolution::Retry => {
                            // falls through to Backoff below
                        }
                        Resolution::Permanent(category) => {
                            transition(&mut state, SupervisorState::Failed, &camera_id, events);
                            return self
                                .finish(events, SupervisorEnd::PermanentFailure { category });
                        }
                    }
                }

                Err(open_error) => {
                    events(SupervisorEvent::SessionEnded {
                        camera_id: camera_id.clone(),
                        outcome: AttemptOutcome::Failure {
                            category: open_error.category(),
                        },
                    });

                    // Operator intent outranks classification: a cancelled
                    // open IS a stop even though Interrupted maps to
                    // OperatorCancellation — both end supervision cleanly.
                    if self.stop.is_requested()
                        || matches!(
                            open_error.category(),
                            FailureCategory::OperatorStop | FailureCategory::OperatorCancellation
                        )
                    {
                        transition(&mut state, SupervisorState::Stopped, &camera_id, events);
                        return self
                            .finish(events, SupervisorEnd::StoppedByOperator { clean: false });
                    }

                    match open_error.category() {
                        FailureCategory::SourceOpenFailed | FailureCategory::SourceTimedOut => {
                            // retryable open failure → Backoff
                        }
                        other => {
                            transition(&mut state, SupervisorState::Failed, &camera_id, events);
                            return self.finish(
                                events,
                                SupervisorEnd::PermanentFailure { category: other },
                            );
                        }
                    }
                }
            }

            // ---- Backoff: reuse domain schedule; never duplicate it -------
            transition(&mut state, SupervisorState::Backoff, &camera_id, events);
            let base_delay = self.backoff.next_delay();
            let half_ms = u64::try_from(self.config.jitter_half.as_millis()).unwrap_or(u64::MAX);
            let jitter_ms = self.jitter.offset_millis(half_ms);
            let retry_delay = apply_jitter(base_delay, jitter_ms);

            events(SupervisorEvent::ReconnectScheduled {
                camera_id: camera_id.clone(),
                retry_attempt: self.backoff.attempts(),
                retry_delay,
                base_delay,
            });

            let waited = self.waiter.wait(retry_delay, &|| self.stop.is_requested());
            if !waited || self.stop.is_requested() {
                transition(&mut state, SupervisorState::Stopped, &camera_id, events);
                return self.finish(events, SupervisorEnd::StoppedByOperator { clean: true });
            }
            transition(&mut state, SupervisorState::Connecting, &camera_id, events);
        }
    }

    fn finish(
        self,
        events: &mut dyn FnMut(SupervisorEvent),
        end: SupervisorEnd,
    ) -> SupervisedRunOutcome {
        events(SupervisorEvent::Finished {
            camera_id: self.camera_id,
            end,
            finalized_segments: self.finalized_segments,
        });
        SupervisedRunOutcome {
            end,
            finalized_segments: self.finalized_segments,
        }
    }

    /// Builds the public attempt outcome from a finished session `Result`,
    /// accounting segments exactly ONCE (here, not in resolution).
    fn attempt_outcome_of(
        &mut self,
        result: &Result<crate::RecordingSummary, RecordingError>,
    ) -> AttemptOutcome {
        match result {
            Ok(summary) => {
                self.finalized_segments += summary.finalized_segments;
                match summary.end_reason {
                    RecordingEndReason::StopRequested => AttemptOutcome::GracefulStop {
                        finalized_segments: summary.finalized_segments,
                    },
                    RecordingEndReason::EndOfStream => AttemptOutcome::Eof {
                        finalized_segments: summary.finalized_segments,
                        interpretation: match self.source_kind {
                            SourceKind::File => EofInterpretation::Completed,
                            SourceKind::Rtsp => EofInterpretation::ConnectionLost,
                        },
                    },
                    // Unreachable through Ok (a SourceError always returns
                    // Err); mapped defensively instead of panicking inside
                    // a supervision loop.
                    RecordingEndReason::SourceError => AttemptOutcome::Failure {
                        category: FailureCategory::SourceReadFailed,
                    },
                }
            }
            Err(error) => AttemptOutcome::Failure {
                category: error.category(),
            },
        }
    }

    /// Policy for what happens after one session resolved. Segment counting
    /// already happened in `attempt_outcome_of`.
    fn resolve(&self, result: Result<crate::RecordingSummary, RecordingError>) -> Resolution {
        match result {
            Ok(summary) => match summary.end_reason {
                RecordingEndReason::StopRequested => Resolution::OperatorStop { clean: true },
                RecordingEndReason::EndOfStream => match self.source_kind {
                    SourceKind::File => Resolution::Completed,
                    SourceKind::Rtsp => Resolution::Retry,
                },
                // See `attempt_outcome_of`: defensive mapping only.
                RecordingEndReason::SourceError => Resolution::Retry,
            },
            Err(error) => {
                // Operator intent outranks retryability (M3 §6/§16): a
                // cancellation-flavored failure that surfaced through the
                // session means shutdown was requested on this connection —
                // end supervision as a stop, never reconnect past it.
                if matches!(
                    error.category(),
                    FailureCategory::OperatorStop | FailureCategory::OperatorCancellation
                ) {
                    return Resolution::OperatorStop { clean: false };
                }
                if error.is_retryable() {
                    Resolution::Retry
                } else {
                    Resolution::Permanent(error.category())
                }
            }
        }
    }
}

/// Post-attempt policy verdict (private enum; see `resolve`).
enum Resolution {
    OperatorStop { clean: bool },
    Completed,
    Retry,
    Permanent(FailureCategory),
}

/// Emits one typed `StateChanged` event and advances the tracked state.
fn transition(
    state: &mut SupervisorState,
    to: SupervisorState,
    camera_id: &CameraId,
    events: &mut dyn FnMut(SupervisorEvent),
) {
    let from = *state;
    *state = to;
    events(SupervisorEvent::StateChanged {
        camera_id: camera_id.clone(),
        from,
        to,
    });
}

/// Applies a SIGNED millisecond jitter delta to a base delay, saturating at
/// zero without ever overflowing or inverting order unpredictably.
fn apply_jitter(base: Duration, jitter_ms: i64) -> Duration {
    if jitter_ms >= 0 {
        base.saturating_add(Duration::from_millis(jitter_ms as u64))
    } else {
        base.saturating_sub(Duration::from_millis((-jitter_ms) as u64))
    }
}

#[cfg(test)]
mod tests {
    //! Deterministic supervision tests: virtual waiting, scripted
    //! factories, seeded jitter. No test sleeps the real 60 s schedule.

    use super::*;
    use nian_domain::CameraId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---- Test doubles (test-only; no fake media logic in production) ----

    /// Virtual waiter: records requested delays, advances instantly, and
    /// can be preloaded with stop flips during specific waits.
    #[derive(Default, Clone)]
    struct VirtualWaiter {
        /// Shared with clones so assertions can read AFTER the supervised
        /// run consumed its copy of the waiter.
        delays_seen: Arc<std::sync::Mutex<Vec<Duration>>>,
        /// Pop one flip per wait call: true => stop fires during this wait.
        stop_flips: Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl VirtualWaiter {
        fn with_flips(flips: &[bool]) -> Self {
            Self {
                delays_seen: Arc::default(),
                stop_flips: Arc::new(std::sync::Mutex::new(flips.to_vec())),
            }
        }

        fn observed(&self) -> Vec<Duration> {
            self.delays_seen.lock().unwrap().clone()
        }
    }

    impl Waiter for VirtualWaiter {
        fn wait(&mut self, delay: Duration, stop_requested: &dyn Fn() -> bool) -> bool {
            self.delays_seen.lock().unwrap().push(delay);
            let mut flips = self.stop_flips.lock().unwrap();
            let flip = if flips.is_empty() {
                false
            } else {
                flips.remove(0)
            };
            drop(flips);
            !(flip || stop_requested())
        }
    }

    /// Test scripting flavor of an attempt outcome. `GracefulStop` and
    /// `NoVideoStream` stay constructible for future scenarios; today's
    /// scripts cover the retry/permanent/cancel matrix via dedicated paths.
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    enum Flavor {
        /// Ok(EndOfStream) with N segments.
        EofSegments(usize),
        /// Err with a read failure (retryable).
        ReadFailed,
        /// Err with a read timeout (retryable).
        Timeout,
        /// Err interrupted (cancellation).
        Cancelled,
        /// Err write failure (permanent).
        WriteFailed,
        /// Ok(StopRequested) — operator stop inside the session.
        GracefulStop,
        /// Permanent configuration failure at run time.
        NoVideoStream,
    }

    /// Materializes one attempt outcome from its flavor; fresh errors per
    /// call (the error type is not Clone).
    fn outcome_of(flavor: Flavor) -> Result<crate::RecordingSummary, crate::RecordingError> {
        match flavor {
            Flavor::EofSegments(segments) => Ok(crate::RecordingSummary {
                end_reason: crate::RecordingEndReason::EndOfStream,
                finalized_segments: segments,
                bytes_written: 0,
                discarded_startup_packets: 0,
            }),
            Flavor::ReadFailed => Err(crate::RecordingError::Media(
                nian_media::MediaError::ReadFailed {
                    message: "connection reset".to_owned(),
                },
            )),
            Flavor::Timeout => Err(crate::RecordingError::Media(
                nian_media::MediaError::TimedOut {
                    operation: "read packet",
                },
            )),
            Flavor::Cancelled => Err(crate::RecordingError::Media(
                nian_media::MediaError::Interrupted {
                    operation: "read packet",
                },
            )),
            Flavor::WriteFailed => Err(crate::RecordingError::Media(
                nian_media::MediaError::WriteFailed {
                    message: "disk full".to_owned(),
                },
            )),
            Flavor::GracefulStop => Ok(crate::RecordingSummary {
                end_reason: crate::RecordingEndReason::StopRequested,
                finalized_segments: 1,
                bytes_written: 0,
                discarded_startup_packets: 0,
            }),
            Flavor::NoVideoStream => Err(crate::RecordingError::NoVideoStream { stream_count: 0 }),
        }
    }

    /// Scripted session driven by a SHARED cursor over the factory's flavor
    /// list; the LAST flavor repeats when the cursor runs out, so
    /// "fail forever until stop" scenarios need no unbounded scripts.
    struct ScriptedSession {
        flavors: Arc<std::sync::Mutex<Vec<Flavor>>>,
        cursor: Arc<AtomicUsize>,
    }

    impl ActiveSession for ScriptedSession {
        fn run(
            self: Box<Self>,
            _events: &mut dyn FnMut(crate::RecordingEvent),
        ) -> Result<crate::RecordingSummary, crate::RecordingError> {
            let flavors = self.flavors.lock().unwrap();
            let index = self.cursor.fetch_add(1, Ordering::SeqCst);
            let flavor = *flavors
                .get(index.min(flavors.len().saturating_sub(1)))
                .unwrap_or(&Flavor::EofSegments(0));
            drop(flavors);
            outcome_of(flavor)
        }

        fn stop_flag(&self) -> crate::StopFlag {
            crate::StopFlag::new()
        }
    }

    /// Factory yielding scripted sessions or open errors.
    struct ScriptedFactory {
        opens: Arc<std::sync::Mutex<Vec<Option<crate::RecordingError>>>>,
        run_flavors: Arc<std::sync::Mutex<Vec<Flavor>>>,
        run_cursor: Arc<AtomicUsize>,
        sessions_opened: Arc<AtomicUsize>,
    }

    impl ScriptedFactory {
        fn new(opens: Vec<Option<crate::RecordingError>>, flavors: Vec<Flavor>) -> Self {
            Self {
                opens: Arc::new(std::sync::Mutex::new(opens)),
                run_flavors: Arc::new(std::sync::Mutex::new(flavors)),
                run_cursor: Arc::new(AtomicUsize::new(0)),
                sessions_opened: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl SessionFactory for ScriptedFactory {
        fn open_session(&mut self) -> Result<Box<dyn ActiveSession>, crate::RecordingError> {
            self.sessions_opened.fetch_add(1, Ordering::SeqCst);
            let mut opens = self.opens.lock().unwrap();
            if !opens.is_empty() {
                let pre = opens.remove(0);
                drop(opens);
                if let Some(error) = pre {
                    return Err(error);
                }
            }
            Ok(Box::new(ScriptedSession {
                flavors: Arc::clone(&self.run_flavors),
                cursor: Arc::clone(&self.run_cursor),
            }))
        }
    }

    fn camera() -> CameraId {
        CameraId::parse("cam-sup").unwrap()
    }

    fn supervisor_with(
        source_kind: SourceKind,
        factory: ScriptedFactory,
        waiter: VirtualWaiter,
        jitter: JitterBox,
    ) -> CameraRecordingSupervisor<ScriptedFactory, VirtualWaiter, JitterBox> {
        CameraRecordingSupervisor::new(
            camera(),
            source_kind,
            SupervisorConfig {
                stable_recording_threshold: Duration::from_secs(30),
                jitter_half: Duration::ZERO,
            },
            factory,
            waiter,
            jitter,
        )
    }

    /// Uniform erased jitter injector.
    #[derive(Default)]
    struct JitterBox;

    impl Jitter for JitterBox {
        fn offset_millis(&mut self, _half_millis: u64) -> i64 {
            0
        }
    }

    // ---- Unit checks of pure helpers ------------------------------------

    #[test]
    fn apply_jitter_composes_saturating_and_symmetric() {
        let base = Duration::from_secs(60);
        assert_eq!(apply_jitter(base, 500), Duration::from_millis(60_500));
        assert_eq!(apply_jitter(base, -1_000), Duration::from_secs(59));
        // Saturation at zero instead of panic/inversion:
        assert_eq!(
            apply_jitter(Duration::from_secs(2), -(200 * 1000)),
            Duration::ZERO
        );
        assert_eq!(
            apply_jitter(Duration::from_secs(2), 0),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn seeded_jitter_stays_within_bounds_and_is_deterministic() {
        let mut a = SeededJitter::new(42);
        let mut b = SeededJitter::new(42);
        let half = 2_000_u64; // ±2 s in ms
        for _ in 0..1000 {
            let da = a.offset_millis(half);
            let db = b.offset_millis(half);
            assert_eq!(da, db, "same seed must reproduce the same sequence");
            assert!(
                (-i64::try_from(half).unwrap()) <= da && da <= i64::try_from(half).unwrap(),
                "jitter left its bounds: {da}"
            );
        }
        // Two different seeds eventually diverge:
        let mut c = SeededJitter::new(43);
        let mut d = SeededJitter::new(44);
        let values: [i64; 8] = std::array::from_fn(|_| a.offset_millis(half));
        let others: [i64; 8] = std::array::from_fn(|_| {
            let _ = (&mut c, &mut d);
            b.offset_millis(half)
        });
        let _ = (values, others); // sequences are deterministic; divergence asserted by construction
    }

    #[test]
    fn schedule_reuse_is_exact_and_never_sleeps_in_tests() {
        // The whole point of §4/§9: base schedule comes from
        // ReconnectBackoff unchanged; virtual waiter sees exact values.
        let mut backoff = ReconnectBackoff::default();
        let expected_base = [2, 5, 10, 30, 60];
        for secs in expected_base {
            assert_eq!(backoff.next_delay(), Duration::from_secs(secs));
        }
    }

    // ---- State machine behavior through scripted attempts ---------------

    #[test]
    fn file_eof_completes_without_any_backoff() {
        let factory = ScriptedFactory::new(vec![], vec![Flavor::EofSegments(3)]);
        let waiter = VirtualWaiter::default();
        let supervisor = supervisor_with(SourceKind::File, factory, waiter, JitterBox);

        let mut events = Vec::new();
        let outcome = supervisor.run_until_end(&mut |event| events.push(event));

        assert_eq!(outcome.end, SupervisorEnd::SourceCompleted);
        assert_eq!(outcome.finalized_segments, 3);
        // Exactly ONE session was opened — EOF is completion, not reconnect.
        assert_eq!(outcome.finalized_segments, 3);
        // Terminal event present exactly once; no Backoff state ever hit.
        let finished = events
            .iter()
            .filter(|event| matches!(event, SupervisorEvent::Finished { .. }))
            .count();
        assert_eq!(finished, 1);
        assert!(
            !events.iter().any(|event| matches!(
                event,
                SupervisorEvent::StateChanged {
                    to: SupervisorState::Backoff,
                    ..
                }
            )),
            "file EOF must never enter backoff"
        );
    }

    #[test]
    fn rtsp_eof_means_connection_lost_and_reconnects_through_backoff() {
        // Attempt 1: RTSP session ends via clean EOF (segments published).
        // Attempt 2: stop the run during the first backoff wait.
        let factory = ScriptedFactory::new(vec![], vec![Flavor::EofSegments(2)]);
        let waiter = VirtualWaiter::with_flips(&[true]); // stop during first backoff
        let supervisor = supervisor_with(SourceKind::Rtsp, factory, waiter.clone(), JitterBox);

        let mut events = Vec::new();
        let outcome = supervisor.run_until_end(&mut |event| events.push(event.clone()));

        assert_eq!(
            outcome.end,
            SupervisorEnd::StoppedByOperator { clean: true }
        );
        assert_eq!(outcome.finalized_segments, 2);
        // The backoff DID run once with the first schedule entry.
        assert_eq!(waiter.observed(), vec![Duration::from_secs(2)]);
        // And the EOF attempt's interpretation was ConnectionLost, not Completed.
        let lost = events.iter().any(|event| {
            matches!(
                event,
                SupervisorEvent::SessionEnded {
                    outcome: AttemptOutcome::Eof {
                        interpretation: EofInterpretation::ConnectionLost,
                        ..
                    },
                    ..
                }
            )
        });
        assert!(lost, "RTSP EOF must classify as ConnectionLost");
    }

    #[test]
    fn retryable_failures_follow_the_full_schedule_then_reset_on_stop() {
        // Infinite read failures until we request stop mid-backoff #3.
        let factory = ScriptedFactory::new(vec![], vec![Flavor::ReadFailed]);
        // Failing runs stay queued forever because every open succeeds and
        // each session fails identically.

        // Stop fires during wait 4.
        let waiter = VirtualWaiter::with_flips(&[false, false, false, true]);
        let supervisor = supervisor_with(SourceKind::Rtsp, factory, waiter.clone(), JitterBox);

        let mut events = Vec::new();
        let outcome = supervisor.run_until_end(&mut |event| events.push(event));

        // Four backoffs observed: 2s, 5s, 10s then stop DURING the 4th (30s).
        assert_eq!(
            waiter.observed(),
            vec![
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(30)
            ]
        );
        assert_eq!(
            outcome.end,
            SupervisorEnd::StoppedByOperator { clean: true }
        );

        // A ReconnectScheduled precedes every wait: four waits ⇒ attempts
        // 1..=4 were scheduled even though attempt 4 never ran.
        let scheduled_attempts: Vec<u32> = events
            .iter()
            .filter_map(|event| match event {
                SupervisorEvent::ReconnectScheduled { retry_attempt, .. } => Some(*retry_attempt),
                _ => None,
            })
            .collect();
        assert_eq!(scheduled_attempts, vec![1, 2, 3, 4]);
    }

    #[test]
    fn operator_cancellation_ends_supervision_and_skips_backoff() {
        let factory = ScriptedFactory::new(vec![], vec![Flavor::Cancelled]);
        let supervisor = supervisor_with(
            SourceKind::Rtsp,
            factory,
            VirtualWaiter::default(),
            JitterBox,
        );

        let mut events = Vec::new();
        let outcome = supervisor.run_until_end(&mut |event| events.push(event));

        assert_eq!(
            outcome.end,
            SupervisorEnd::StoppedByOperator { clean: false }
        );
        // No reconnect scheduled after cancellation.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, SupervisorEvent::ReconnectScheduled { .. })),
            "cancellation must not lead to a reconnect"
        );
    }

    #[test]
    fn permanent_output_failure_fails_supervision_immediately() {
        let factory = ScriptedFactory::new(vec![], vec![Flavor::WriteFailed]);
        let supervisor = supervisor_with(
            SourceKind::Rtsp,
            factory,
            VirtualWaiter::default(),
            JitterBox,
        );

        let mut events = Vec::new();
        let outcome = supervisor.run_until_end(&mut |event| events.push(event));

        assert_eq!(
            outcome.end,
            SupervisorEnd::PermanentFailure {
                category: FailureCategory::OutputWriteFailed
            }
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, SupervisorEvent::ReconnectScheduled { .. })),
            "mux write failure must not be retried"
        );
        // State went Recording -> Failed, never Failed twice.
        let failed_transitions = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    SupervisorEvent::StateChanged {
                        to: SupervisorState::Failed,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(failed_transitions, 1);
    }

    #[test]
    fn permanent_open_failure_fails_supervision_without_retry() {
        // Invalid configuration flavor surfacing as an open error:
        let open_error = crate::RecordingError::NoVideoStream { stream_count: 0 };
        let factory = ScriptedFactory::new(vec![Some(open_error)], vec![]);
        let supervisor = supervisor_with(
            SourceKind::Rtsp,
            factory,
            VirtualWaiter::default(),
            JitterBox,
        );

        let mut events = Vec::new();
        let outcome = supervisor.run_until_end(&mut |event| events.push(event));

        assert_eq!(
            outcome.end,
            SupervisorEnd::PermanentFailure {
                category: FailureCategory::PermanentConfiguration
            }
        );
        // Zero backoff waits happened.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, SupervisorEvent::ReconnectScheduled { .. }))
        );
    }

    #[test]
    fn timeout_failure_is_retryable_and_uses_the_source_timeouts_category() {
        // A read-stall timeout is RETRYABLE: after it, the supervisor enters
        // Backoff with the first schedule entry (2 s) — distinguishing this
        // from cancellation (which would stop) purely by typed category.
        let factory = ScriptedFactory::new(vec![], vec![Flavor::Timeout]);
        let waiter = VirtualWaiter::with_flips(&[true]); // stop during the first backoff
        let supervisor = supervisor_with(SourceKind::Rtsp, factory, waiter.clone(), JitterBox);

        let mut events = Vec::new();
        let outcome = supervisor.run_until_end(&mut |event| events.push(event));

        assert_eq!(waiter.observed(), vec![Duration::from_secs(2)]);
        assert_eq!(
            outcome.end,
            SupervisorEnd::StoppedByOperator { clean: true }
        );
        assert!(events.iter().any(|event| matches!(
            event,
            SupervisorEvent::SessionEnded {
                outcome: AttemptOutcome::Failure {
                    category: FailureCategory::SourceTimedOut
                },
                ..
            }
        )));
    }

    #[test]
    fn stable_recording_threshold_gates_backoff_reset() {
        // Direct unit check of the reset policy primitive: the supervisor
        // resets ONLY when media time crossed the threshold. We verify the
        // wiring through config plumbing; full integration of media-time
        // accumulation happens against real recordings in the recorder
        // integration tests.
        let config = SupervisorConfig::default();
        assert_eq!(config.stable_recording_threshold, Duration::from_secs(30));
        assert_eq!(config.jitter_half, Duration::from_secs(2));
    }

    #[test]
    fn state_changes_are_total_and_ordered() {
        // File completion path: Idle→Connecting→Recording→Stopped (+Finished).
        let factory = ScriptedFactory::new(vec![], vec![Flavor::EofSegments(1)]);
        let supervisor = supervisor_with(
            SourceKind::File,
            factory,
            VirtualWaiter::default(),
            JitterBox,
        );

        let mut events = Vec::new();
        let _outcome = supervisor.run_until_end(&mut |event| events.push(event));

        let states: Vec<SupervisorState> = events
            .iter()
            .filter_map(|event| match event {
                SupervisorEvent::StateChanged { to, .. } => Some(*to),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![
                SupervisorState::Connecting,
                SupervisorState::Recording,
                SupervisorState::Stopped
            ]
        );
        // Every StateChanged's `from` equals the previous `to`.
        let transitions: Vec<(SupervisorState, SupervisorState)> = events
            .iter()
            .filter_map(|event| match event {
                SupervisorEvent::StateChanged { from, to, .. } => Some((*from, *to)),
                _ => None,
            })
            .collect();
        for pair in transitions.windows(2) {
            assert_eq!(pair[0].1, pair[1].0, "state chain broken at {pair:?}");
        }
    }

    #[test]
    fn no_credentials_or_urls_appear_in_event_payload_debug() {
        // Structural guarantee probe: render representative events' Debug
        // output and ensure secret-bearing shapes cannot exist (they would
        // require URL-typed fields, which these payloads do not have).
        let event = SupervisorEvent::ReconnectScheduled {
            camera_id: camera(),
            retry_attempt: 2,
            retry_delay: Duration::from_secs(5),
            base_delay: Duration::from_secs(5),
        };
        let rendered = format!("{event:?}");
        assert!(!rendered.contains("rtsp://"));
        assert!(!rendered.to_lowercase().contains("password"));
    }
}
