# ADR 0019 — Independent Motion Event Capture

- Status: Accepted for RC49
- Date: 2026-09-13

## Context

RC47/RC48 projected persisted motion onto the normal Recording controller. A transient motion owner could start the same recorder used by manual Recording, manual Start could promote that slot, Event Review correlated the Event back to Timeline footage by timestamp, and the five-second “pre-roll” was only a playback seek when earlier manual footage happened to exist. Physical Tapo C200 acceptance showed this ownership model was wrong: motion automation visibly occupied manual recording controls/Timeline, and a motion-triggered recorder opened only after the Event arrived, so the beginning of a movement could be absent.

The product requirement is stricter: manual Recording and motion Event footage are independent products. A user-controlled manual recording must remain continuous and exclusively controlled by manual Desired/Start/Stop, while a motion Event must receive its own clip containing footage from before the trigger through the end/post-roll. Continuous motion may exceed one clip, but no event clip part may exceed five minutes.

## Decision

Nian Vision uses a separate Event capture plane. While Event monitoring Desired is On, a dedicated recording controller continuously packet-copies short H.264 segments into `<storage_root>/.nian/event-buffer`. It has no access to the manual Recording controller or persistent Recording Desired. The buffer is short-lived and pruned independently.

A newly persisted aggregate MotionStarted creates an Event episode over that already-running buffer. The requested window includes approximately five seconds before the trigger and five seconds after aggregate motion becomes idle. Motion reactivation cancels pending finalization. The media worker concatenates selected finalized segments into `<storage_root>/.nian/event-clips/<camera>/<episode>/clip.mkv` without transcoding. Each source segment is rebased to a cumulative output timeline; packet spacing is preserved and a mux-level duration guard keeps every clip part below the five-minute hard limit. Long continuous motion creates continuation episodes with boundary overlap and preserves the root Event aliases so every part remains discoverable.

Event aliases are immutable files under `by-event/<event-id>/<episode-id>.json`. Event Review never queries the manual Recording index for footage. An Event may resolve to multiple clip parts and the UI exposes Clip N/M navigation. Event playback validates real, non-symlink directory/file identity and pins the active episode against retention.

Event clips follow Event/history age, defaulting to 30 days when no max age is configured. They count with manual recording bytes for quota pressure; quota cleanup removes eligible old Event clips first and leaves active Event playback untouched. Manual Timeline inventory continues to ignore `.nian`, so event clips never appear as Timeline recordings.

## Consequences

Manual Recording and Event capture can run simultaneously without starting, stopping, promoting or mutating each other. True pre-roll no longer depends on manual footage. The cost is an additional RTSP ingest while Event capture is enabled, so camera connection limits remain relevant when Live View and manual Recording are also active. RC49 intentionally accepts that cost rather than reintroducing shared ownership. A future shared-ingest fan-out may optimize transport without changing these logical ownership boundaries.

The Event buffer continuously writes short packet-copy segments while Event monitoring is enabled. This is deliberate NVR behavior required for real pre-trigger video; the buffer remains bounded and is not retained as Timeline media.

## Rejected alternatives

- **Reuse/promote the manual recorder:** rejected because motion automation then owns user controls and Timeline lifecycle.
- **Start a recorder only after MotionStarted:** rejected because footage before the trigger cannot be reconstructed.
- **Treat Timeline seek as pre-roll:** rejected because it is conditional on unrelated manual footage and does not create an Event-owned artifact.
- **Store only one mutable alias per Event:** rejected because continuous motion can require multiple bounded immutable parts.
