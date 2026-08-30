# ADR-0009: Local playback transport and browser container strategy

* Status: Accepted
* Milestone: M6

## Context

Nian Vision records finalized packet-copy MKV files. The M6 desktop UI needs to
browse those recordings, seek inside them and move between adjacent files while
recording may continue in parallel. The embedded browser cannot be assumed to
play every MKV/codec combination reliably, and the existing media-worker NDJSON
channel is a bounded control protocol rather than a video transport.

The M4 storage contract remains non-negotiable:

* filesystem media is authoritative;
* `nian-index` is a disposable query cache;
* React must not receive arbitrary local paths;
* partials and recovery-control artifacts are never playback sources;
* a stale index row cannot authorize a file open.

M6 also introduces a retention race: a user may play an old finalized recording
at the same time retention decides to delete it. Relying on an open OS file handle
is not portable because Unix permits unlinking open files.

## Decision

### Timeline identity and validation

Timeline/day/range/adjacency queries use `nian-index`. The canonical relative
recording path is the rebuild-stable application identity. React treats that
string as opaque; it is never accepted as an unchecked filesystem path.

Before playback opens, the application resolves the index row and reconstructs
the expected path through `RecordingsLayout`. It validates the exact
camera/year/month/day/filename grammar, normal-or-recovered classification,
sequence/time identity, regular-file type and indexed size using
`symlink_metadata` on the root and every component. Symlink traversal, absolute
paths, dot components, missing objects and replacements fail closed. Every later
HTTP media request repeats source-identity revalidation.

### Media preparation

Tauri does not link FFmpeg. A one-shot `playback.prepare` operation runs inside
`nian-media-worker` through the existing versioned NDJSON control channel. The
host supplies only paths it derived after trusted validation; the frontend never
supplies worker paths directly.

For the current Tapo C200 target, H.264 video is supported without transcoding.
AAC audio is copied when present; incompatible audio may be omitted. The worker
uses direct libav FFI through `nian-media-ffmpeg`, never an `ffmpeg` CLI, decoder
or encoder. Packets are copied into a fragmented MP4 using:

```text
frag_keyframe + empty_moov + default_base_moof + global_sidx
```

The MP4 is created under a unique application cache session directory with
exclusive ownership. It is outside the recording tree, never indexed as footage,
never retention-managed, and removed when the session closes/expires. Startup
also removes stale `session-*` cache directories.

This first implementation prepares the complete finalized recording once when a
session opens. That trades open latency and temporary cache bytes for a simple,
bounded browser transport and makes later seeking independent of replaying the
MKV from the beginning.

### Playback transport

`PlaybackController` owns a loopback HTTP server bound only to `127.0.0.1` on an
ephemeral port. Each successful open creates an unguessable UUID token mapping to
exactly one validated recording/session. The endpoint shape is:

```text
http://127.0.0.1:<ephemeral>/playback/<session-uuid>
```

There is no directory listing, arbitrary path parameter, storage-root disclosure
or `0.0.0.0` listener. Host/Origin patterns are restricted to the expected local
desktop clients where practical. The server bounds active sessions, concurrent
requests, header bytes and streaming buffers. GET and HEAD are supported with a
single HTTP byte range; invalid ranges return 416.

The NDJSON channel carries only lifecycle/metadata/errors. Video/audio bytes are
never encoded into `Envelope` messages.

### Seeking

The prepared MP4 is fragmented at keyframes and includes a global `sidx`. HTML
`<video>` seeks therefore become byte-range requests against a random-access
representation. The source is packet-copy media, so seeking is GOP/keyframe
granular and is not promised to be frame-perfect.

The current M6 seek path does not invoke a new libav seek operation for every UI
seek because the complete source has already been packet-copy remuxed exactly once
into the indexed MP4 representation. A seek does not decode or remux from source
time zero. If a later milestone replaces the prepared cache with on-demand remux
streaming, that implementation must seek the source through libav to a preceding
keyframe rather than scan from the beginning.

### Duration enrichment

M4 rebuilds intentionally leave `media_duration_ms` NULL. `playback.prepare`
returns trusted container duration when available. The application revalidates
the source after media work and updates duration only if the indexed
camera/path/kind/start/sequence/size identity still matches. A failed SQLite
writeback does not affect media playback. Rebuilding the disposable index may
discard enriched duration, after which it is lazily rediscovered.

### Playback retention pin

A session acquires `PlaybackPin` only after filesystem validation succeeds. The
pin is keyed by canonical relative recording identity and is separate from
`CameraLease`.

`CameraLease` remains responsible for live partials, recovery mutation and writer
ownership. Finalized historical recordings can be played read-only while the same
camera continues recording new segments.

Retention checks playback pins while selecting work and again immediately before
the filesystem delete. Thus a session that opens after a retention plan selected
the file but before commit still prevents deletion and increments
`skipped_playback`. An admitted HTTP media request also keeps the session alive:
idle expiry cannot drop the pin mid-response, and explicit close/configuration
change becomes a deferred close until the last active request finishes. New
requests are rejected once close is requested. The final request guard releases
the session and pin deterministically.

## Consequences

Positive:

* filesystem authority and rebuild-stable recording identity survive M6;
* React never gains arbitrary local-file capability;
* browser compatibility improves without transcoding;
* Range seeking does not push media bytes through control IPC;
* playback and ongoing recording do not contend on `CameraLease`;
* retention behavior is explicit and portable across Windows and Unix.

Costs/trade-offs:

* opening a long recording may require a full packet-copy remux before playback;
* the prepared MP4 consumes bounded temporary disk space for the session;
* unsupported video codecs fail rather than transcode in M6;
* unsupported audio may be omitted;
* frame-perfect seeking and live RTSP preview remain outside M6.

## Rejected alternatives

**`file://` or absolute path in React.** Rejected because it turns the frontend
into a privileged local-file opener and bypasses filesystem/index revalidation.

**Serve the recording tree over HTTP.** Rejected because a directory/path server
would unnecessarily enlarge the local file-reading boundary.

**Stream media through NDJSON/base64.** Rejected because control IPC is bounded
and unsuitable for multi-megabyte video payloads/backpressure.

**Transcode to a universally playable codec.** Rejected for M6 because the target
H.264 stream is already browser-compatible after container remux and transcoding
would add CPU cost, quality loss and encoder/licensing surface.

**Depend on open-file deletion failure.** Rejected because Unix unlink semantics
make that behavior unsuitable as the product retention contract.

## Scope

This ADR covers finalized local recording playback only. It does not authorize
live RTSP viewing, clip export, thumbnail/motion generation, M7 lifecycle work,
M8 distribution, M9 simultaneous multi-camera recording, M10 ONVIF, AI or cloud
features.
