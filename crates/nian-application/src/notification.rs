//! Bounded local desktop notification projection for persisted motion events.
//!
//! EventIndex remains authoritative. This module consumes only newly committed
//! normalized transitions and deliberately has no ONVIF dependency.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

use crate::EventHistoryKind;

pub const NOTIFICATION_QUEUE_CAPACITY: usize = 32;
pub const MOTION_NOTIFICATION_RATE_LIMIT_SECS: i64 = 15;
pub const MAX_NOTIFICATION_RATE_LIMIT_ENTRIES: usize = 128;
pub const NOTIFICATION_DELIVERY_TIMEOUT: Duration = Duration::from_secs(3);
const NOTIFICATION_DELIVERY_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
    #[error("notification delivery could not be terminated")]
    DeliveryTerminationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationDeliveryPoll {
    Pending,
    Delivered,
}

/// One owned native notification delivery. `poll` must be non-blocking and
/// `terminate` must synchronously settle the owned native operation.
pub trait DesktopNotificationDelivery: Send {
    fn poll(&mut self) -> Result<NotificationDeliveryPoll, NotificationError>;
    fn terminate(&mut self) -> Result<(), NotificationError>;
}

pub trait DesktopNotifier: Send + Sync {
    fn supported(&self) -> bool;
    fn start_motion(
        &self,
        request: &MotionNotificationRequest,
    ) -> Result<Box<dyn DesktopNotificationDelivery>, NotificationError>;
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
    pub delivery_timeouts: AtomicU64,
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

    fn accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
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
    delivery_start_gate: Arc<Mutex<()>>,
    running: Arc<AtomicBool>,
    counters: Arc<NotificationCounters>,
    last_notified_event_id: Arc<AtomicU64>,
    delivery_timeout: Duration,
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
        Self::with_delivery_timeout(notifier, enabled, NOTIFICATION_DELIVERY_TIMEOUT)
    }

    fn with_delivery_timeout(
        notifier: Arc<dyn DesktopNotifier>,
        enabled: bool,
        delivery_timeout: Duration,
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
            delivery_start_gate: Arc::new(Mutex::new(())),
            running: Arc::new(AtomicBool::new(false)),
            counters,
            last_notified_event_id: Arc::new(AtomicU64::new(0)),
            delivery_timeout,
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
        {
            // Linearize admission closure against delivery start. If the worker already
            // owns this gate, that delivery started before suspension; otherwise no
            // stale queued signal can cross the closure boundary into native delivery.
            let _start_guard = self
                .delivery_start_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.admission.detach();
            self.running.store(false, Ordering::Release);
        }
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
        let delivery_start_gate = self.delivery_start_gate.clone();
        let running = self.running.clone();
        let admission = self.admission.clone();
        let counters = self.counters.clone();
        let last_notified_event_id = self.last_notified_event_id.clone();
        let delivery_timeout = self.delivery_timeout;
        let join = thread::Builder::new()
            .name("nian-notifications".to_owned())
            .spawn(move || {
                let mut limiter = NotificationRateLimiter::default();
                while let Ok(signal) = receiver.recv() {
                    if !running.load(Ordering::Acquire)
                        || !admission.accepting()
                        || !admission.enabled()
                    {
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
                    let mut delivery = {
                        let _start_guard = delivery_start_gate
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if !running.load(Ordering::Acquire)
                            || !admission.accepting()
                            || !admission.enabled()
                        {
                            continue;
                        }
                        match notifier.start_motion(&request) {
                            Ok(delivery) => delivery,
                            Err(_) => {
                                counters.notifier_failures.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        }
                    };
                    let deadline = Instant::now() + delivery_timeout;
                    let delivered = loop {
                        if !running.load(Ordering::Acquire) || !admission.accepting() {
                            if delivery.terminate().is_err() {
                                counters.notifier_failures.fetch_add(1, Ordering::Relaxed);
                            }
                            break false;
                        }
                        match delivery.poll() {
                            Ok(NotificationDeliveryPoll::Delivered) => break true,
                            Ok(NotificationDeliveryPoll::Pending) => {}
                            Err(_) => {
                                counters.notifier_failures.fetch_add(1, Ordering::Relaxed);
                                break false;
                            }
                        }
                        if Instant::now() >= deadline {
                            if delivery.terminate().is_err() {
                                counters.notifier_failures.fetch_add(1, Ordering::Relaxed);
                            }
                            // Publish the timeout only after termination has settled so observers
                            // cannot see a completed timeout while the native delivery is still live.
                            counters.delivery_timeouts.fetch_add(1, Ordering::Release);
                            break false;
                        }
                        thread::sleep(NOTIFICATION_DELIVERY_POLL_INTERVAL);
                    };
                    if delivered {
                        last_notified_event_id.store(request.event_id, Ordering::Release);
                        counters.shown.fetch_add(1, Ordering::Relaxed);
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
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use std::time::Duration;

    use super::*;

    struct ImmediateDelivery {
        fail: bool,
        finished: bool,
    }

    impl DesktopNotificationDelivery for ImmediateDelivery {
        fn poll(&mut self) -> Result<NotificationDeliveryPoll, NotificationError> {
            if self.finished {
                return Ok(NotificationDeliveryPoll::Delivered);
            }
            self.finished = true;
            if self.fail {
                Err(NotificationError::DeliveryFailed)
            } else {
                Ok(NotificationDeliveryPoll::Delivered)
            }
        }

        fn terminate(&mut self) -> Result<(), NotificationError> {
            self.finished = true;
            Ok(())
        }
    }

    struct FakeNotifier {
        supported: bool,
        fail: AtomicBool,
        started: AtomicUsize,
    }

    impl DesktopNotifier for FakeNotifier {
        fn supported(&self) -> bool {
            self.supported
        }

        fn start_motion(
            &self,
            _request: &MotionNotificationRequest,
        ) -> Result<Box<dyn DesktopNotificationDelivery>, NotificationError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(ImmediateDelivery {
                fail: self.fail.load(Ordering::SeqCst),
                finished: false,
            }))
        }
    }

    #[derive(Default)]
    struct ControlledNotifierState {
        started: Mutex<Vec<u64>>,
        delivered: Mutex<Vec<u64>>,
        terminations: AtomicUsize,
    }

    struct ControlledNotifier {
        block_event_id: AtomicU64,
        state: Arc<ControlledNotifierState>,
    }

    impl ControlledNotifier {
        fn blocking(event_id: u64) -> Self {
            Self {
                block_event_id: AtomicU64::new(event_id),
                state: Arc::new(ControlledNotifierState::default()),
            }
        }

        fn allow_all(&self) {
            self.block_event_id.store(0, Ordering::SeqCst);
        }

        fn started(&self, event_id: u64) -> bool {
            self.state
                .started
                .lock()
                .is_ok_and(|events| events.contains(&event_id))
        }

        fn delivered(&self, event_id: u64) -> bool {
            self.state
                .delivered
                .lock()
                .is_ok_and(|events| events.contains(&event_id))
        }
    }

    struct ControlledDelivery {
        event_id: u64,
        blocked: bool,
        settled: bool,
        state: Arc<ControlledNotifierState>,
    }

    impl DesktopNotificationDelivery for ControlledDelivery {
        fn poll(&mut self) -> Result<NotificationDeliveryPoll, NotificationError> {
            if self.settled {
                return Ok(NotificationDeliveryPoll::Delivered);
            }
            if self.blocked {
                return Ok(NotificationDeliveryPoll::Pending);
            }
            self.settled = true;
            if let Ok(mut delivered) = self.state.delivered.lock() {
                delivered.push(self.event_id);
            }
            Ok(NotificationDeliveryPoll::Delivered)
        }

        fn terminate(&mut self) -> Result<(), NotificationError> {
            if !self.settled {
                self.settled = true;
                self.state.terminations.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    impl DesktopNotifier for ControlledNotifier {
        fn supported(&self) -> bool {
            true
        }

        fn start_motion(
            &self,
            request: &MotionNotificationRequest,
        ) -> Result<Box<dyn DesktopNotificationDelivery>, NotificationError> {
            if let Ok(mut started) = self.state.started.lock() {
                started.push(request.event_id);
            }
            Ok(Box::new(ControlledDelivery {
                event_id: request.event_id,
                blocked: self.block_event_id.load(Ordering::SeqCst) == request.event_id,
                settled: false,
                state: self.state.clone(),
            }))
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
        for _ in 0..200 {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            predicate(),
            "condition did not become true within the test bound"
        );
    }

    fn assert_lifecycle_call_finishes(
        dispatcher: Arc<NotificationDispatcher>,
        operation: impl FnOnce(&NotificationDispatcher) -> Result<(), NotificationError>
        + Send
        + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = operation(&dispatcher);
            let _ = tx.send(result);
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(500))
                .expect("lifecycle call exceeded its deterministic test bound")
                .is_ok()
        );
        worker.join().unwrap();
    }

    #[test]
    fn motion_started_notifies_motion_ended_does_not_and_rate_limit_is_per_camera() {
        let notifier = Arc::new(FakeNotifier {
            supported: true,
            fail: AtomicBool::new(false),
            started: AtomicUsize::new(0),
        });
        let dispatcher = NotificationDispatcher::new(notifier.clone(), true).unwrap();
        let sink = dispatcher.sink();
        sink.try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionEnded));
        sink.try_publish(signal("cam-a", 2, 101, EventHistoryKind::MotionStarted));
        sink.try_publish(signal("cam-a", 3, 105, EventHistoryKind::MotionStarted));
        sink.try_publish(signal("cam-b", 4, 105, EventHistoryKind::MotionStarted));
        wait_until(|| dispatcher.counters().shown.load(Ordering::SeqCst) == 2);
        assert_eq!(notifier.started.load(Ordering::SeqCst), 2);
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
    fn notifier_failure_does_not_stop_dispatch() {
        let notifier = Arc::new(FakeNotifier {
            supported: true,
            fail: AtomicBool::new(true),
            started: AtomicUsize::new(0),
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
        dispatcher.shutdown().unwrap();
    }

    #[test]
    fn blocking_delivery_cannot_hang_suspend() {
        let notifier = Arc::new(ControlledNotifier::blocking(1));
        let dispatcher = Arc::new(
            NotificationDispatcher::with_delivery_timeout(
                notifier.clone(),
                true,
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        dispatcher
            .sink()
            .try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionStarted));
        wait_until(|| notifier.started(1));

        assert_lifecycle_call_finishes(dispatcher, |dispatcher| dispatcher.suspend());
        assert_eq!(notifier.state.terminations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn blocking_delivery_cannot_hang_shutdown() {
        let notifier = Arc::new(ControlledNotifier::blocking(1));
        let dispatcher = Arc::new(
            NotificationDispatcher::with_delivery_timeout(
                notifier.clone(),
                true,
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        dispatcher
            .sink()
            .try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionStarted));
        wait_until(|| notifier.started(1));

        assert_lifecycle_call_finishes(dispatcher, |dispatcher| dispatcher.shutdown());
        assert_eq!(notifier.state.terminations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn resume_does_not_replay_old_queued_generation() {
        let notifier = Arc::new(ControlledNotifier::blocking(1));
        let dispatcher = NotificationDispatcher::with_delivery_timeout(
            notifier.clone(),
            true,
            Duration::from_secs(30),
        )
        .unwrap();
        let sink = dispatcher.sink();
        sink.try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionStarted));
        wait_until(|| notifier.started(1));
        sink.try_publish(signal("cam-b", 2, 100, EventHistoryKind::MotionStarted));

        dispatcher.suspend().unwrap();
        notifier.allow_all();
        dispatcher.resume().unwrap();
        sink.try_publish(signal("cam-c", 3, 200, EventHistoryKind::MotionStarted));
        wait_until(|| notifier.delivered(3));
        assert!(!notifier.started(2));
        dispatcher.shutdown().unwrap();
    }

    #[test]
    fn blocked_delivery_keeps_event_ingestion_non_blocking_and_queue_bounded() {
        let notifier = Arc::new(ControlledNotifier::blocking(1));
        let dispatcher = NotificationDispatcher::with_delivery_timeout(
            notifier.clone(),
            true,
            Duration::from_secs(30),
        )
        .unwrap();
        let sink = dispatcher.sink();
        sink.try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionStarted));
        wait_until(|| notifier.started(1));

        let started = Instant::now();
        for event_id in 2..=(NOTIFICATION_QUEUE_CAPACITY as u64 + 20) {
            sink.try_publish(signal(
                &format!("cam-{event_id}"),
                event_id,
                200 + event_id as i64,
                EventHistoryKind::MotionStarted,
            ));
        }
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(
            dispatcher
                .counters()
                .dropped_queue_full
                .load(Ordering::SeqCst)
                > 0
        );
        dispatcher.shutdown().unwrap();
    }

    #[test]
    fn delivery_timeout_is_isolated_and_worker_delivers_future_events() {
        let notifier = Arc::new(ControlledNotifier::blocking(1));
        let dispatcher = NotificationDispatcher::with_delivery_timeout(
            notifier.clone(),
            true,
            Duration::from_millis(30),
        )
        .unwrap();
        let sink = dispatcher.sink();
        sink.try_publish(signal("cam-a", 1, 100, EventHistoryKind::MotionStarted));
        wait_until(|| {
            dispatcher
                .counters()
                .delivery_timeouts
                .load(Ordering::SeqCst)
                == 1
        });
        assert_eq!(notifier.state.terminations.load(Ordering::SeqCst), 1);

        notifier.allow_all();
        sink.try_publish(signal("cam-b", 2, 200, EventHistoryKind::MotionStarted));
        wait_until(|| notifier.delivered(2));
        assert_eq!(dispatcher.counters().shown.load(Ordering::SeqCst), 1);
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
            started: AtomicUsize::new(0),
        });
        let dispatcher = NotificationDispatcher::new(notifier, true).unwrap();
        assert!(!dispatcher.settings().motion_notifications_enabled);
        assert!(!dispatcher.settings().supported);
        assert!(dispatcher.set_enabled(true).is_err());
        dispatcher.shutdown().unwrap();
    }
}
