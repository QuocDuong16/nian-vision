//! Tiny safe boundary around Windows desktop lifecycle primitives.
//!
//! This crate is the sole M7 exception to the workspace's normal `unsafe`
//! prohibition. All Win32 pointers, callback lifetime handling, job-object
//! ownership, and FFI calls stay here; callers receive only safe Rust APIs.

use std::process::{Child, Command};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    Suspend,
    Resume,
}

#[derive(Debug, thiserror::Error)]
pub enum PowerEventError {
    #[error("Windows power notification registration failed with code {0}")]
    Registration(u32),
    #[error("Windows power notifications are unsupported on this platform")]
    Unsupported,
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Power::{
        DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, PowerRegisterSuspendResumeNotification,
        PowerUnregisterSuspendResumeNotification,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, GetCurrentProcess};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND,
    };

    use super::{PowerEvent, PowerEventError};

    struct KillOnCloseJob(HANDLE);

    // HANDLE is process-local opaque state. Access is only through Win32 APIs,
    // which permit this job handle to be used from arbitrary process threads.
    unsafe impl Send for KillOnCloseJob {}
    unsafe impl Sync for KillOnCloseJob {}

    impl Drop for KillOnCloseJob {
        fn drop(&mut self) {
            // SAFETY: this handle is owned by this wrapper and was returned by
            // CreateJobObjectW. Process termination also closes it automatically.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn last_error_code() -> i32 {
        io::Error::last_os_error().raw_os_error().unwrap_or(1)
    }

    fn create_worker_job() -> Result<KillOnCloseJob, i32> {
        // SAFETY: null security/name requests an unnamed job with default ACL.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(last_error_code());
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let info_size = u32::try_from(std::mem::size_of_val(&limits)).map_err(|_| 1)?;
        // SAFETY: `limits` is the documented structure for this information
        // class and remains live for the duration of the call.
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast::<c_void>(),
                info_size,
            )
        };
        if configured == 0 {
            let error = last_error_code();
            // SAFETY: configuration failed before ownership leaves this scope.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(error);
        }
        // SAFETY: GetCurrentProcess returns the calling process pseudo-handle.
        // Joining before any worker spawn removes the spawn/assign orphan race:
        // ordinary child processes inherit this job membership automatically.
        if unsafe { AssignProcessToJobObject(handle, GetCurrentProcess()) } == 0 {
            let error = last_error_code();
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(error);
        }
        Ok(KillOnCloseJob(handle))
    }

    static WORKER_JOB: OnceLock<Result<KillOnCloseJob, i32>> = OnceLock::new();

    pub fn initialize_worker_containment() -> io::Result<()> {
        WORKER_JOB
            .get_or_init(create_worker_job)
            .as_ref()
            .map(|_| ())
            .map_err(|code| io::Error::from_raw_os_error(*code))
    }

    pub fn configure_worker_command(command: &mut std::process::Command) {
        // Media workers are background sidecars. Never create a visible console
        // window when the GUI host launches them on Windows.
        command.creation_flags(CREATE_NO_WINDOW);
    }

    pub fn contain_worker(child: &std::process::Child) -> io::Result<()> {
        let job = WORKER_JOB.get_or_init(create_worker_job);
        let job = job
            .as_ref()
            .map_err(|code| io::Error::from_raw_os_error(*code))?;
        let process = child.as_raw_handle().cast::<c_void>();
        let mut already_contained = 0;
        // SAFETY: both process and job handles remain valid for this call.
        if unsafe { IsProcessInJob(process, job.0, &mut already_contained) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if already_contained != 0 {
            return Ok(());
        }
        // SAFETY: Child owns a live process handle. The process-wide job handle
        // remains open until desktop termination, when KILL_ON_JOB_CLOSE kills
        // any still-running assigned media workers.
        if unsafe { AssignProcessToJobObject(job.0, process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    type Callback = Box<dyn Fn(PowerEvent) + Send + Sync + 'static>;

    pub struct Subscription {
        registration: HPOWERNOTIFY,
        context: *mut Callback,
    }

    // The registration is process-global infrastructure. Win32 may invoke the
    // callback on a system thread; the boxed callback itself is Send + Sync.
    unsafe impl Send for Subscription {}
    unsafe impl Sync for Subscription {}

    unsafe extern "system" fn power_callback(
        context: *const c_void,
        event_type: u32,
        _setting: *const c_void,
    ) -> u32 {
        if context.is_null() {
            return 0;
        }
        let event = match event_type {
            PBT_APMSUSPEND => Some(PowerEvent::Suspend),
            // Always emitted on resume. Ignore PBT_APMRESUMESUSPEND so one
            // physical wake cannot schedule two restoration passes.
            PBT_APMRESUMEAUTOMATIC => Some(PowerEvent::Resume),
            _ => None,
        };
        if let Some(event) = event {
            // SAFETY: context is allocated before registration and remains live
            // until successful unregistration. Panic is contained at the FFI
            // boundary and never unwinds into Windows.
            let callback = unsafe { &*(context.cast::<Callback>()) };
            let _ = catch_unwind(AssertUnwindSafe(|| callback(event)));
        }
        0
    }

    impl Subscription {
        pub fn new<F>(callback: F) -> Result<Self, PowerEventError>
        where
            F: Fn(PowerEvent) + Send + Sync + 'static,
        {
            let context = Box::into_raw(Box::new(Box::new(callback) as Callback));
            let parameters = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(power_callback),
                Context: context.cast::<c_void>(),
            };
            let mut registration = std::ptr::null_mut::<c_void>();
            // SAFETY: DEVICE_NOTIFY_CALLBACK requires recipient to point at a
            // live DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS for the duration of this
            // call. Win32 copies Callback/Context into the registration.
            let code = unsafe {
                PowerRegisterSuspendResumeNotification(
                    DEVICE_NOTIFY_CALLBACK,
                    (&parameters as *const DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS)
                        .cast_mut()
                        .cast::<c_void>(),
                    &mut registration,
                )
            };
            if code != 0 {
                // SAFETY: registration failed, therefore Windows did not retain
                // the callback context and ownership remains entirely local.
                unsafe { drop(Box::from_raw(context)) };
                return Err(PowerEventError::Registration(code));
            }
            Ok(Self {
                registration: registration as HPOWERNOTIFY,
                context,
            })
        }
    }

    impl Drop for Subscription {
        fn drop(&mut self) {
            // SAFETY: registration was returned by the matching register call.
            let code = unsafe { PowerUnregisterSuspendResumeNotification(self.registration) };
            if code == 0 {
                // SAFETY: successful unregistration guarantees Windows will no
                // longer call with this context, so ownership can be reclaimed.
                unsafe { drop(Box::from_raw(self.context)) };
            } else {
                // Deliberately leak the tiny callback allocation on unregister
                // failure. Freeing it would risk a use-after-free from Win32.
                self.context = std::ptr::null_mut();
            }
        }
    }

    pub fn subscribe<F>(callback: F) -> Result<Subscription, PowerEventError>
    where
        F: Fn(PowerEvent) + Send + Sync + 'static,
    {
        Subscription::new(callback)
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    use super::{PowerEvent, PowerEventError};

    pub struct Subscription;

    pub fn subscribe<F>(_callback: F) -> Result<Subscription, PowerEventError>
    where
        F: Fn(PowerEvent) + Send + Sync + 'static,
    {
        Err(PowerEventError::Unsupported)
    }

    pub fn initialize_worker_containment() -> io::Result<()> {
        Ok(())
    }

    pub fn configure_worker_command(_command: &mut std::process::Command) {}

    pub fn contain_worker(_child: &std::process::Child) -> io::Result<()> {
        Ok(())
    }
}

pub use imp::Subscription as PowerEventSubscription;

pub fn subscribe<F>(callback: F) -> Result<PowerEventSubscription, PowerEventError>
where
    F: Fn(PowerEvent) + Send + Sync + 'static,
{
    imp::subscribe(callback)
}

/// Initializes the process-wide worker Job Object before any media worker can
/// be spawned. On Windows, child processes then inherit containment atomically.
/// This is a no-op on non-Windows targets.
pub fn initialize_worker_process_containment() -> std::io::Result<()> {
    imp::initialize_worker_containment()
}

/// Configures a media-worker child process for background execution. On Windows
/// this suppresses the transient console window; other platforms are unchanged.
pub fn configure_worker_command(command: &mut Command) {
    imp::configure_worker_command(command);
}

/// Assigns a media-worker child to a process-wide Windows Job Object whose
/// members are terminated automatically if the desktop process dies without a
/// graceful teardown. This is a no-op on non-Windows targets.
pub fn contain_worker_process(child: &Child) -> std::io::Result<()> {
    imp::contain_worker(child)
}
