//! Bounded local desktop notification projection for persisted motion events.
//!
//! EventIndex remains authoritative. This module consumes only newly committed
//! normalized transitions and deliberately has no ONVIF dependency.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

use crate::EventHistoryKind;

pub const NOTIFICATION_QUEUE_CAPACITY: usize = 32;
pub const MOTION_NOTIFICATION_RATE_LIMIT_SECS: i64 = 15;
pub const MAX_NOTIFICATION_RATE_LIMIT_ENTRIES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedEventSignal {
    pub event_id: u64,
    pub camera_id: String,
    pub camera_display_name: String,
    pub kind: EventHistoryKind,
    pub received_time_utc: DateTime<Utc>,
}

pub trait PersistedEventSink: Send + Sync {
    /// Must remain non-blocking. Queue saturation is a UX drop, never Event
    /// ingestion backpressure.
    fn try_publish(&self, signal: PersistedEventSignal);
}

#[derive(Debug, Default)]
pub struct NoopPersistedEventSink;

impl PersistedEventSink for NoopPersistedEventSink {
    fn try_publish(&self, _signal: PersistedEventSignal) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MotionNotificationRequest {
    pub event_id: u64,
    pub camera_id: String,
    pub camera_display_name: String,
    pub received_time_utc: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum NotificationError {
    #[error("desktop notifications are unsupported")]
    Unsupported,
    #[error("notification runtime is unavailable")]
    RuntimeUnavailable,
    #[error("notification worker could not start")]
    WorkerStart,
    #[error("notification worker could not join")]
    WorkerJoin,
    #[error("notification delivery failed")]
    DeliveryFailed,
}

pub trait DesktopNotifier: Send + Sync {
    fn supported(&self) -> bool;
    fn show_motion(&self, request: &MotionNotificationRequest) -> Result<(), NotificationError>;
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct NotificationSettingsDto {
    pub motion_notifications_enabled: bool,
    pub supported: bool,
}

#[derive(Debug, Default)]
pub struct NotificationCounters {
    pub enqueued: AtomicU64,
    pub dropped_queue_full: AtomicU64,
    pub dropped_rate_limited: AtomicU64,
    pub shown: AtomicU64,
    pub notifier_failures: AtomicU64,
}

struct AdmissionState {
    sender: Option<SyncSender<PersistedEventSignal>>,
}

pub struct NotificationAdmission {
    enabled: AtomicBool,
    accepting: AtomicBool,
    state: Mutex<AdmissionState>,
    counters: Arc<NotificationCounters>,
}

impl std::fmt::Debug for NotificationAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationAdmission")
            .field("enabled", &self.enabled.load(Ordering::Acquire))
            .field("accepting", &self.accepting.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl NotificationAdmission {
    fn new(enabled: bool, counters: Arc<NotificationCounters>) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            accepting: AtomicBool::new(false),
            state: Mutex::new(AdmissionState { sender: None }),
            counters,
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn attach(&self, sender: SyncSender<PersistedEventSignal>) -> Result<(), NotificationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotificationError::RuntimeUnavailable)?;
        state.sender = Some(sender);
        self.accepting.store(true, Ordering::Release);
        Ok(())
    }

    fn detach(&self) {
        self.accepting.store(false, Ordering::Release);
        if let Ok(mut state) = self.state.lock() {
            state.sender.take();
        }
    }
}

impl PersistedEventSink for NotificationAdmission {
    fn try_publish(&self, signal: PersistedEventSignal) {
        if signal.kind != EventHistoryKind::MotionStarted
            || !self.enabled.load(Ordering::Acquire)
            || !self.accepting.load(Ordering::Acquire)
        {
            return;
        }
        let sender = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.sender.as_ref().cloned());
        let Some(sender) = sender else {
            return;
        };
        match sender.try_send(signal) {
            Ok(()) => {
                self.counters.enqueued.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                self.counters
                    .dropped_queue_full
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

struct DispatcherWorker {
    join: JoinHandle<()>,
}

pub struct NotificationDispatcher {
    notifier: Arc<dyn DesktopNotifier>,
    admission: Arc<NotificationAdmission>,
    worker: Mutex<Option<DispatcherWorker>>,
    running: Arc<AtomicBool>,
    counters: Arc<NotificationCounters>,
    last_notified_event_id: Arc<AtomicU64>,
}

impl std::fmt::Debug for NotificationDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationDispatcher")
            .field("supported", &self.notifier.supported())
            .field("enabled", &self.admission.enabled())
            .finish_non_exhaustive()
    }
}

impl NotificationDispatcher {
    pub fn new(
        notifier: Arc<dyn DesktopNotifier>,
        enabled: bool,
    ) -> Result<Self, NotificationError> {
        let counters = Arc::new(NotificationCounters::default());
        let admission = Arc::new(NotificationAdmission::new(
            enabled && notifier.supported(),
            counters.clone(),
        ));
        let dispatcher = Self {
            notifier,
            admission,
            worker: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            counters,
            last_notified_event_id: Arc::new(AtomicU64::new(0)),
        };
        dispatcher.resume()?;
        Ok(dispatcher)
    }

    pub fn sink(&self) -> Arc<dyn PersistedEventSink> {
        self.admission.clone()
    }

    pub fn settings(&self) -> NotificationSettingsDto {
        NotificationSettingsDto {
            motion_notifications_enabled: self.admission.enabled(),
            supported: self.notifier.supported(),
        }
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<NotificationSettingsDto, NotificationError> {
        if enabled && !self.notifier.supported() {
            return Err(NotificationError::Unsupported);
        }
        self.admission.set_enabled(enabled);
        Ok(self.settings())
    }

    pub fn counters(&self) -> &Arc<NotificationCounters> {
        &self.counters
    }

    pub fn last_notified_event_id(&self) -> Option<u64> {
        match self.last_notified_event_id.load(Ordering::Acquire) {
            0 => None,
            value => Some(value),
        }
    }

    pub fn suspend(&self) -> Result<(), NotificationError> {
        self.running.store(false, Ordering::Release);
        self.admission.detach();
        let worker = self
            .worker
            .lock()
            .map_err(|_| NotificationError::RuntimeUnavailable)?
            .take();
        if let Some(worker) = worker {
            worker
                .join
                .join()
                .map_err(|_| NotificationError::WorkerJoin)?;
        }
        Ok(())
    }

    pub fn resume(&self) -> Result<(), NotificationError> {
        let mut worker_slot = self
            .worker
            .lock()
            .map_err(|_| NotificationError::RuntimeUnavailable)?;
        if worker_slot.is_some() {
            return Ok(());
        }
        let (sender, receiver) = mpsc::sync_channel(NOTIFICATION_QUEUE_CAPACITY);
        self.running.store(true, Ordering::Release);
        self.admission.attach(sender)?;
        let notifier = self.notifier.clone();
        let running = self.running.clone();
        let admission = self.admission.clone();
        let counters = self.counters.clone();
        let last_notified_event_id = self.last_notified_event_id.clone();
        let join = thread::Builder::new()
            .name("nian-notifications".to_owned())
            .spawn(move || {
                let mut limiter = NotificationRateLimiter::default();
                while let Ok(signal) = receiver.recv() {
                    if !running.load(Ordering::Acquire) || !admission.enabled() {
                        continue;
                    }
                    if !limiter.allow(&signal.camera_id, signal.received_time_utc) {
                        counters
                            .dropped_rate_limited
                            .fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let request = MotionNotificationRequest {
                        event_id: signal.event_id,
                        camera_id: signal.camera_id,
                        camera_display_name: signal.camera_display_name,
                        received_time_utc: signal.received_time_utc,
                    };
                    match notifier.show_motion(&request) {
                        Ok(()) => {
                            last_notified_event_id.store(request.event_id, Ordering::Release);
                            counters.shown.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            counters.notifier_failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
            .map_err(|_| NotificationError::WorkerStart)?;
        *worker_slot = Some(DispatcherWorker { join });
        Ok(())
    }

    pub fn shutdown(&self) -> Result<(), NotificationError> {
        self.suspend()
    }
}

impl Drop for NotificationDispatcher {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

#[derive(Default)]
struct NotificationRateLimiter {
    last_by_camera: HashMap<String, DateTime<Utc>>,
}

impl NotificationRateLimiter {
    fn allow(&mut self, camera_id: &str, received_time_utc: DateTime<Utc>) -> bool {
        if let Some(previous) = self.last_by_camera.get(camera_id)
            && received_time_utc
                .signed_duration_since(*previous)
                .num_seconds()
                < MOTION_NOTIFICATION_RATE_LIMIT_SECS
        {
            return false;
        }
        if !self.last_by_camera.contains_key(camera_id)
            && self.last_by_camera.len() >= MAX_NOTIFICATION_RATE_LIMIT_ENTRIES
            && let Some(oldest) = self
                .last_by_camera
                .iter()
                .min_by_key(|(_, value)| **value)
                .map(|(camera, _)| camera.clone())
        {
            self.last_by_camera.remove(&oldest);
        }
        self.last_by_camera
            .insert(camera_id.to_owned(), received_time_utc);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;

    use super::*;

    struct FakeNotifier {
        supported: bool,
        fail: AtomicBool,
        shown: AtomicUsize,
    }

    impl DesktopNotifier for FakeNotifier {
        fn supported(&self) -> bool {
            self.supported
        }

        fn show_motion(
            &self,
            _request: &MotionNotificationRequest,
        ) -> Result<(), NotificationError> {
            self.shown.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err(NotificationError::DeliveryFailed)
            } else {
                Ok(())
            }
        }
    }

    fn signal(
        camera: &str,
        event_id: u64,
        second: i64,
        kind: EventHistoryKind,
    ) -> PersistedEventSignal {
        PersistedEventSignal {
            event_id,
            camera_id: camera.to_owned(),
            camera_display_name: camera.to_owned(),
            kind,
            received_time_utc: DateTime::from_timestamp(second, 0).unwrap(),
        }
    }

    fn wait_until(predicate: impl Fn() -> bool) {
        for _ in 0..100 {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn motion_started_notifies_motion_ended_does_not_and_rate_limit_is_per_camera() {
        let notifier = Arc::new(FakeNotifier {
            supported: true,
            fail: AtomicBool::new(false),
            shown: AtomicUsize::new(0),
        });
        let dispatcher = NotificationDispatcher::new(notifier.clone(), true).unwrap();
        let sink = dispatcher.sink();
        sink.try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionEnded));
        sink.try_publish(signal("cam-a", 2, 101, EventHistoryKind::MotionStarted));
        sink.try_publish(signal("cam-a", 3, 105, EventHistoryKind::MotionStarted));
        sink.try_publish(signal("cam-b", 4, 105, EventHistoryKind::MotionStarted));
        wait_until(|| dispatcher.counters().shown.load(Ordering::SeqCst) == 2);
        assert_eq!(notifier.shown.load(Ordering::SeqCst), 2);
        assert_eq!(
            dispatcher
                .counters()
                .dropped_rate_limited
                .load(Ordering::SeqCst),
            1
        );
        dispatcher.shutdown().unwrap();
    }

    #[test]
    fn notifier_failure_does_not_stop_dispatch_and_suspend_drops_stale_admission() {
        let notifier = Arc::new(FakeNotifier {
            supported: true,
            fail: AtomicBool::new(true),
            shown: AtomicUsize::new(0),
        });
        let dispatcher = NotificationDispatcher::new(notifier.clone(), true).unwrap();
        let sink = dispatcher.sink();
        sink.try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionStarted));
        wait_until(|| {
            dispatcher
                .counters()
                .notifier_failures
                .load(Ordering::SeqCst)
                == 1
        });
        notifier.fail.store(false, Ordering::SeqCst);
        sink.try_publish(signal("cam-b", 2, 100, EventHistoryKind::MotionStarted));
        wait_until(|| dispatcher.counters().shown.load(Ordering::SeqCst) == 1);
        dispatcher.suspend().unwrap();
        sink.try_publish(signal("cam-c", 3, 100, EventHistoryKind::MotionStarted));
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(notifier.shown.load(Ordering::SeqCst), 2);
        dispatcher.resume().unwrap();
        sink.try_publish(signal("cam-c", 4, 200, EventHistoryKind::MotionStarted));
        wait_until(|| dispatcher.counters().shown.load(Ordering::SeqCst) == 2);
        dispatcher.shutdown().unwrap();
    }

    #[test]
    fn full_admission_queue_drops_without_blocking_event_ingestion() {
        let counters = Arc::new(NotificationCounters::default());
        let admission = NotificationAdmission::new(true, counters.clone());
        let (sender, _receiver) = mpsc::sync_channel(1);
        admission.attach(sender).unwrap();

        admission.try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionStarted));
        admission.try_publish(signal("cam-b", 2, 100, EventHistoryKind::MotionStarted));

        assert_eq!(counters.enqueued.load(Ordering::SeqCst), 1);
        assert_eq!(counters.dropped_queue_full.load(Ordering::SeqCst), 1);
        admission.detach();
    }

    #[test]
    fn unsupported_notifier_cannot_be_enabled() {
        let notifier = Arc::new(FakeNotifier {
            supported: false,
            fail: AtomicBool::new(false),
            shown: AtomicUsize::new(0),
        });
        let dispatcher = NotificationDispatcher::new(notifier, true).unwrap();
        assert!(!dispatcher.settings().motion_notifications_enabled);
        assert!(!dispatcher.settings().supported);
        assert!(dispatcher.set_enabled(true).is_err());
        dispatcher.shutdown().unwrap();
    }
}
