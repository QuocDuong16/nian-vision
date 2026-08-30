# ADR-0010: Desktop production lifecycle, persisted recording intent and Windows worker containment

* Status: Accepted
* Milestone: M7

## Context

M0-M6 established safe media isolation, supervised recording, authoritative
settings, native credentials, filesystem-first storage and local playback. The
desktop still lacked the lifecycle contract expected from an always-on NVR:

* closing the window must not accidentally stop recording;
* a second launch must activate the existing process instead of creating another
  desktop backend;
* launch-at-login must start hidden and must remain consistent with the persisted
  preference;
* a recording the user intended to keep running must survive ordinary desktop
  restart and sleep/wake;
* suspend, resume and Quit must not race new Start/Probe/Playback work;
* an explicit Quit must own worker teardown through completion; and
* hard Windows desktop termination must not leave `nian-media-worker` processes
  orphaned.

Session-only recording state is insufficient for this. Runtime state such as
`Recording` or `Failed` describes what the controller is doing now; it does not
describe what the user asked the application to keep doing.

## Decision

### One desktop process and tray-owned window lifetime

`tauri-plugin-single-instance` is registered before every other Tauri plugin so a
secondary process exits before backend resources initialize. A normal secondary
launch activates the existing main window. A duplicate launch carrying the exact
`--startup-hidden` marker does not surface the window.

The main window is created hidden. Normal interactive startup explicitly shows,
unminimizes and focuses it; autostart leaves it hidden. `CloseRequested` hides the
window while the Rust lifecycle is Running or Suspending. Only the Quitting state
allows the close to proceed. The tray owns Open, recording status, Stop recording
and explicit Quit actions.

### Rust-authoritative lifecycle admission

`nian-application::DesktopLifecycle` owns three states:

```text
Running -> Suspending -> Running
   |            |
   +----------> Quitting
```

Quitting is terminal. A desktop `control_gate` serializes lifecycle transitions
with operations whose correctness depends on lifecycle admission or recording
ownership. Start, camera mutation, Probe, playback open and settings mutation
must prove Running while holding that gate before they can commit new work.

Suspend and Quit close subsystem admission before potentially blocking teardown.
Power callbacks themselves only enqueue a typed event; the application power
dispatcher performs the bounded orchestration off the Win32 callback thread.

### Persisted desired recording is separate from runtime status

`nian-settings` schema v2 adds `cameras.recording_enabled` and
`application_settings.launch_at_login`. A partial unique index permits at most one
camera row with `recording_enabled=1`, preserving the single-recording M5 product
constraint in authoritative storage rather than only in process memory.

Start ordering is:

```text
validate/prepare recording
-> persist Desired=On
-> start RecordingController
```

If runtime startup fails, Desired remains On. The failure is visible as runtime
`Failed`, and the next startup/resume may retry restoration through the same
normal `RecordingController` path.

User Stop ordering is the reverse intent boundary:

```text
persist Desired=Off
-> request runtime stop
```

A crash during runtime teardown therefore cannot resurrect a recording the user
explicitly disabled. Suspend and Quit stop runtime ownership without changing the
persisted desired flag.

The UI presents Desired and Runtime separately. `Desired: On / Runtime: Failed`
is a valid, observable state rather than being collapsed into "recording off".

### Launch-at-login preference and OS registration converge transactionally

Autostart uses the exact `--startup-hidden` argument. On settings change, the host
prepares all ordinary settings/playback changes first, changes the OS autostart
registration, and only then commits authoritative settings. If settings
persistence fails, the OS registration is rolled back. A rollback failure is a
typed `autostart_failed` error and is never reported as convergence.

At startup, persisted `launch_at_login` remains authoritative. The OS registration
is inspected and reconciled to that preference; OS drift does not rewrite the
database preference in the opposite direction.

### Suspend/resume is convergent, not all-or-nothing

Suspend blocks new work, cancels an in-flight probe, blocks new playback sessions
and requests the existing recording controller to stop. Desired recording intent
is untouched.

Resume returns lifecycle admission to Running while still holding the control
gate, expires/resynchronizes playback state, completes ownership of the suspended
recording controller, restores Desired=On through normal Start logic, and reopens
probe admission. Restoration is best-effort per subsystem: for example, a failed
playback index refresh is reported but cannot prevent recording restoration or
leave Probe/Playback admission permanently disabled. A duplicate Resume while
already Running is a no-op and cannot create a second controller run.

### Explicit Quit owns deterministic teardown

The first Quit transition wins and immediately rejects new work. Teardown order is
fixed:

```text
recording shutdown/join
-> playback shutdown
-> probe shutdown
-> unregister/join power event infrastructure
-> process exit
```

If the dedicated shutdown coordinator thread cannot be created, the same bounded
sequence runs synchronously instead of leaving an already-Quitting process wedged.
Normal user Quit does not clear Desired recording intent.

### Windows hard termination uses a kill-on-close Job Object

`nian-platform-windows` is the isolated Win32 lifecycle boundary. Before any
media worker can be spawned, the desktop creates an unnamed Job Object, enables
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and assigns the desktop process itself to
that job. Ordinary worker children therefore inherit job membership atomically,
eliminating the spawn-then-assign orphan race.

Every recording/probe/playback production worker spawn additionally verifies the
child is contained. If containment cannot be proven or assigned, the child is
killed and reaped and the spawn fails closed. When the desktop process terminates
hard, Windows closes the process-owned job handle and kills any surviving workers.

All raw handles, callback pointers and Job Object FFI remain inside
`nian-platform-windows`; application and desktop orchestration keep safe APIs.

## Consequences

* Recording intent now survives ordinary restart and sleep/wake independently of
  transient runtime failures.
* Close-to-tray is no longer equivalent to process exit; explicit Quit is the only
  normal user path that tears down the backend.
* Windows lifecycle correctness adds a small audited `unsafe` boundary, but avoids
  distributing Win32 code across controllers.
* Launch-at-login and settings updates can return typed convergence failures; the
  UI must not claim success when OS registration or rollback fails.
* M7 still preserves the M5 single-camera recording limit. Multi-camera recording
  remains M9 scope; live view, packaging and ONVIF remain later milestones.

## Verification

Automated tests cover activation ordering, close-to-tray policy, exact hidden-start
argument detection, autostart drift/rollback failures, desired restoration and
failure visibility, suspend/resume single-restoration behavior, resume error
convergence, deterministic Quit ordering, settings schema migration/invariants and
UI separation of Desired versus Runtime. The Windows platform crate is
cross-compiled for `x86_64-pc-windows-msvc` to compile-check the Job Object and
power-notification FFI surface.
