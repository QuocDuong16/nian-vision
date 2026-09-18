# ADR 0019 — Independent Motion Event Capture

- Status: Accepted for RC49; burst-boundary refinement accepted for RC50; shared-ingest/pre-roll refinement accepted 2026-09-16
- Date: 2026-09-13; amended 2026-09-16

## Context

RC47/RC48 projected persisted motion onto the normal Recording controller. A transient motion owner could start the same recorder used by manual Recording, manual Start could promote that slot, Event Review correlated the Event back to Timeline footage by timestamp, and the five-second “pre-roll” was only a playback seek when earlier manual footage happened to exist. Physical Tapo C200 acceptance showed this ownership model was wrong: motion automation visibly occupied manual recording controls/Timeline, and a motion-triggered recorder opened only after the Event arrived, so the beginning of a movement could be absent.

The product requirement is stricter: manual Recording and motion Event footage are independent products. A user-controlled manual recording must remain continuous and exclusively controlled by manual Desired/Start/Stop, while a motion Event must receive its own clip containing footage from before the trigger through the end/post-roll. Continuous motion may exceed one clip, but no event clip part may exceed five minutes.

## Decision

Nian Vision keeps Event capture as a separate **logical ownership plane**, but no longer gives it a dedicated always-on RTSP recorder. While Event monitoring Desired is On, the per-camera media worker retains the camera's shared **main ingest** and keeps a bounded compressed-packet pre-roll ring in memory. The ring is keyframe-aware, retains at most about ten seconds, and is hard-capped at 32 MiB per source. It uses referenced FFmpeg packets rather than duplicating payload per consumer. Manual Recording, Focus Live View and Event capture therefore subscribe to the same main ingest generation when their resolved endpoint is identical.

A newly persisted aggregate MotionStarted creates an Event episode and starts the event-only recorder with an atomic subscription that is prefilled from roughly five seconds of the compressed ring before continuing with live packets from the same ingest generation. There is no snapshot→subscribe gap. The first prefilled packet's age establishes a media-time→wall-clock anchor so pre-roll segments receive timestamps corresponding to their actual packet history rather than all pretending to start at `now`. The matching aggregate MotionEnded closes the product event and adds five seconds of post-roll. RC50's burst rule remains unchanged: a later MotionStarted always opens a new independent episode even when the previous episode is still collecting post-roll, so clips may overlap in source time without being merged. Event media is finalized under `<storage_root>/.nian/event-clips/<camera>/<episode>` without transcoding; the mux-level duration guard keeps every clip part below the five-minute hard limit, and continuous motion creates continuation episodes that preserve root Event aliases.

Event aliases are immutable files under `by-event/<event-id>/<episode-id>.json`. Event Review never queries the manual Recording index for footage. An Event may resolve to multiple clip parts and the UI exposes Clip N/M navigation. Event playback validates real, non-symlink directory/file identity and pins the active episode against retention.

Event clips follow Event/history age, defaulting to 30 days when no max age is configured. They count with manual recording bytes for quota pressure; quota cleanup removes eligible old Event clips first and leaves active Event playback untouched. Manual Timeline inventory continues to ignore `.nian`, so event clips never appear as Timeline recordings.

## Consequences

Manual Recording and Event capture can run simultaneously without starting, stopping, promoting or mutating each other. True pre-roll no longer depends on manual footage, and enabling Event monitoring no longer requires continuous disk writes. Transport ownership is shared without merging product ownership: Recording + Focus + Event pre-roll share one main RTSP ingest, while Grid uses the sub ingest when configured; if main and sub resolve to the same endpoint they dedupe to one ingest. v1 motion state itself comes from ONVIF and does not open an RTSP motion-analysis consumer. The current v1 acceptance bound is therefore at most one main plus one sub RTSP source generation per camera, not one connection per feature.

The compressed pre-roll ring exists only in worker memory and is bounded by both duration and bytes. Disk recording starts only for active/pending Event episodes and stops after finalization/post-roll, while the lightweight ingest lease keeps the ring warm for the next trigger. Turning Event Desired Off during active motion, or losing authoritative ONVIF monitoring while motion is active, settles that episode with only the bounded five-second post-roll and revokes stale aggregate-motion authority so no five-minute continuation can be manufactured after monitoring is gone. Suspend/Quit discard transient active/pending episode state after owned recorders settle. Slow realtime subscribers may drop/resync at a keyframe, but reliable recording subscribers fail closed rather than silently losing packets.

## Rejected alternatives

- **Reuse/promote the manual recorder:** rejected because motion automation then owns user controls and Timeline lifecycle.
- **Start a recorder after MotionStarted without a retained compressed ring:** rejected because footage before the trigger cannot be reconstructed. Starting the event recorder after MotionStarted is acceptable only because the shared ingest can atomically prefill it from already-retained compressed packets.
- **Treat Timeline seek as pre-roll:** rejected because it is conditional on unrelated manual footage and does not create an Event-owned artifact.
- **Store only one mutable alias per Event:** rejected because continuous motion can require multiple bounded immutable parts.
