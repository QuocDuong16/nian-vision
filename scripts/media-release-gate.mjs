import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const executable = (name) => process.platform === "win32" && name === "pnpm" ? "pnpm.cmd" : name;

const steps = [
  ["Rust format", "cargo", ["fmt", "--all", "--", "--check"]],
  ["Rust workspace compile", "cargo", ["check", "--workspace", "--all-targets"]],
  ["Rust strict Clippy", "cargo", ["clippy", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"]],
  ["Shared ingest ownership/reconnect", "cargo", ["test", "-p", "nian-media-worker", "ingest::tests::"]],
  [
    "Media diagnostics schema compatibility",
    "cargo",
    ["test", "-p", "nian-application", "camera_worker::tests::media_status_schema_gracefully_defaults_new_diagnostics_fields"],
  ],
  ["Fan-out backpressure isolation", "cargo", ["test", "-p", "nian-media-ffmpeg", "fanout::tests::"]],
  ["HEVC/G.711 original recording and browser hvc1 remux", "cargo", ["test", "-p", "nian-media-ffmpeg", "--test", "media_integration", "hevc_"]],
  ["HEVC/G.711 playback and Event-clip IPC", "cargo", ["test", "-p", "nian-media-worker", "--test", "worker_integration", "ipc_hevc_g711_"]],
  ["Session-scoped WAV HTTP range and revoke", "cargo", ["test", "-p", "nian-application", "g711_audio_sidecar_is_session_scoped_range_served_and_revoked_on_close"]],
  ["ONVIF HEVC-only profile onboarding", "cargo", ["test", "-p", "nian-onvif", "hevc_only_media2_is_compatible_without_contacting_a_fallback"]],
  ["ONVIF unsupported video stays rejected", "cargo", ["test", "-p", "nian-onvif", "all_validated_candidates_with_only_unsupported_video_return_no_compatible_profile"]],
  ["ONVIF safe H264/HEVC recommendation", "cargo", ["test", "-p", "nian-application", "safe_dtos_omit_credentials_and_prepared_draft_owns_transient_secret"]],
  ["Windows FFmpeg HEVC/G.711 release component contract", "node", ["--test", "scripts/release/ffmpeg-cache.test.mjs", "scripts/release/ffmpeg-msys-build.test.mjs", "scripts/release/validate-ffmpeg-components.test.mjs", "scripts/release/windows-release-contract.test.mjs"]],
  ["Local H264/HEVC luma decoder", "cargo", ["test", "-p", "nian-media-ffmpeg", "luma_decoder::tests::"]],
  ["Local scene-motion hysteresis", "cargo", ["test", "-p", "nian-media-worker", "motion::tests::"]],
  ["Local motion ACK and replay safety", "cargo", ["test", "-p", "nian-media-worker", "motion_job::tests::"]],
  ["Local motion IPC contract", "cargo", ["test", "-p", "nian-media-worker", "--test", "worker_integration", "ipc_local_motion_requires_explicit_valid_profile"]],
  ["Local motion exclusive setting migration", "cargo", ["test", "-p", "nian-settings", "local_motion_preference_is_persistent_exclusive"]],
  ["Local motion persistence, event projection and non-spawning diagnostics", "cargo", ["test", "-p", "nian-application", "local_motion_"]],
  ["Tapo ONVIF person topic provenance", "cargo", ["test", "-p", "nian-onvif", "tapo_people_topic_fallback_is_motion_not_a_person_claim"]],
  ["Person-event index migration", "cargo", ["test", "-p", "nian-index", "v2_migration_preserves_ids_visibility"]],
  ["Atomic ONVIF source + aggregate rollback", "cargo", ["test", "-p", "nian-index", "source_and_aggregate_insert_failure_rolls_back"]],
  ["Person-event projection and single capture", "cargo", ["test", "-p", "nian-application", "camera_person_transitions_are_distinct_review_rows"]],

  [
    "Pre-roll wall-clock mapping",
    "cargo",
    [
      "test",
      "-p",
      "nian-recorder",
      "session::tests::prefilled_media_time_backdates_each_segment_consistently_and_never_future_dates",
    ],
  ],
  ["Event monitoring authority reset", "cargo", ["test", "-p", "nian-application", "entering_backoff_revokes_stale_motion_authority"]],
  ["Event capture disable/disconnect lifecycle", "cargo", ["test", "-p", "nian-desktop", "event_capture::tests::"]],
  ["UI typecheck", "pnpm", ["--filter", "nian-ui", "typecheck"]],
  ["UI lint", "pnpm", ["--filter", "nian-ui", "lint"]],
  [
    "WebView media lifecycle",
    "pnpm",
    [
      "--filter",
      "nian-ui",
      "exec",
      "vitest",
      "run",
      "src/lib/mediaLifecycle.test.ts",
      "src/lib/mediaTelemetry.test.ts",
      "src/lib/mediaSoak.test.ts",
      "src/lib/liveMp4.test.ts",
      "src/components/VideoPlayer.test.tsx",
      "src/components/PerformanceMonitor.test.tsx",
      "src/screens/LiveViewScreen.test.tsx",
      "src/screens/CamerasScreen.test.tsx",
      "src/screens/EventReviewScreen.test.tsx",
    ],
  ],
];

for (const [label, command, args] of steps) {
  console.log(`\n==> ${label}`);
  const result = spawnSync(executable(command), args, {
    cwd: root,
    stdio: "inherit",
    env: process.env,
  });
  if (result.error) {
    console.error(`${label} could not start: ${result.error.message}`);
    process.exit(1);
  }
  if (result.status !== 0) {
    console.error(`${label} failed with exit code ${result.status ?? "unknown"}`);
    process.exit(result.status ?? 1);
  }
}

console.log("\nMedia release gate passed.");
