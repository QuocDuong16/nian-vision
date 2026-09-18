import { describe, expect, it } from "vitest";
import {
  MEDIA_SOAK_MAX_SAMPLES,
  appendMediaSoakSample,
  createMediaSoakSample,
  isFreshMediaSoakSnapshot,
  serializeMediaSoakEvidence,
  summarizeMediaSoak,
} from "./mediaSoak";
import type { CameraMediaDiagnostics, PerformanceSnapshot } from "./tauri";

function snapshot(memory: number, cpu: number, workerMemory: number): PerformanceSnapshot {
  return {
    sample_ready: true,
    process_count: 2,
    app_cpu_percent: cpu,
    root_process_cpu_percent: 1,
    system_cpu_percent: 10,
    app_memory_bytes: memory,
    root_process_memory_bytes: memory - workerMemory,
    child_process_memory_bytes: workerMemory,
    system_memory_used_bytes: 8_000,
    system_memory_total_bytes: 16_000,
    system_network_rx_bps: 1234,
    system_network_tx_bps: 567,
    app_network_rx_bps: null,
    app_network_tx_bps: null,
    app_gpu_percent: null,
    system_gpu_percent: null,
    processes: [
      { pid: 1, name: "nian-vision", role: "Desktop host", cpu_percent: 1, memory_bytes: memory - workerMemory, is_root: true },
      { pid: 2, name: "nian-media-worker", role: "Media worker", cpu_percent: 2, memory_bytes: workerMemory, is_root: false },
    ],
  };
}

function media(generations: number, queueBytes: number, ringBytes: number): CameraMediaDiagnostics[] {
  return [{
    camera_id: "front-door",
    worker_available: true,
    generation_starts: generations,
    source_count: 1,
    main_sources: 1,
    sub_sources: 0,
    connecting_sources: 0,
    ready_sources: 1,
    failed_sources: 0,
    subscribers: 1,
    reliable_subscribers: 1,
    realtime_subscribers: 0,
    consumers: { recording: 1, live: 0, motion: 0, event: 0, other: 0 },
    queued_packets: 1,
    queued_bytes: queueBytes,
    dropped_packets: 0,
    pre_roll_packets: 2,
    pre_roll_bytes: ringBytes,
    pre_roll_dropped_packets: 0,
    sources: [],
  }];
}

describe("media soak helpers", () => {
  it("captures bounded media/process metrics and summarizes baseline, peak and reconnect delta", () => {
    const first = createMediaSoakSample(
      snapshot(200, 10, 40),
      { mountedVideoElements: 1, activeMseSessions: 1, bufferedSeconds: 2 },
      media(3, 10, 20),
      1_000,
    );
    const second = createMediaSoakSample(
      snapshot(260, 35, 70),
      { mountedVideoElements: 2, activeMseSessions: 2, bufferedSeconds: 4.5 },
      media(5, 25, 35),
      6_000,
    );

    expect(first.mediaWorkerMemoryBytes).toBe(40);
    expect(first.packetBufferBytes).toBe(30);
    expect(second.generationStarts).toBe(5);

    expect(summarizeMediaSoak([first, second])).toEqual({
      idleRecovery: null,
      sampleCount: 2,
      durationSeconds: 5,
      coldBaselineAppMemoryBytes: 200,
      warmBaselineAppMemoryBytes: null,
      baselineAppMemoryBytes: 200,
      currentAppMemoryBytes: 260,
      peakAppMemoryBytes: 260,
      peakMediaWorkerMemoryBytes: 70,
      peakAppCpuPercent: 35,
      peakPacketBufferBytes: 60,
      peakSystemNetworkRxBps: 1234,
      peakMountedVideoElements: 2,
      peakActiveMseSessions: 2,
      peakBufferedSeconds: 4.5,
      generationStartsDelta: 2,
      mediaUnavailableSamples: 0,
      maxSampleGapSeconds: 5,
      memoryTrendBytesPerHour: 43_200,
      recentMemorySpanBytes: 60,
      recentWindowSeconds: 5,
    });
  });

  it("exports versioned aggregate evidence without camera/source identity", () => {
    const sample = createMediaSoakSample(
      snapshot(240, 12, 50),
      { mountedVideoElements: 1, activeMseSessions: 1, bufferedSeconds: 1.25 },
      media(4, 12, 34),
      10_000,
    );
    const serialized = serializeMediaSoakEvidence([sample], 20_000, 10_000, 10_000);
    const evidence = JSON.parse(serialized) as { schemaVersion: number; exportedAtMs: number; idleMarkedAtMs: number; warmBaselineMarkedAtMs: number; samples: unknown[] };

    expect(evidence.schemaVersion).toBe(1);
    expect(evidence.exportedAtMs).toBe(20_000);
    expect(evidence.idleMarkedAtMs).toBe(10_000);
    expect(evidence.warmBaselineMarkedAtMs).toBe(10_000);
    expect(evidence.samples).toHaveLength(1);
    expect(serialized).not.toContain("front-door");
    expect(serialized).not.toContain("rtsp://");
  });

  it("derives 30s and 120s idle checkpoints from captured samples instead of wall-clock timers", () => {
    const samples = [
      createMediaSoakSample(
        snapshot(200, 5, 20),
        { mountedVideoElements: 0, activeMseSessions: 0, bufferedSeconds: 0 },
        [],
        0,
      ),
      createMediaSoakSample(
        snapshot(230, 5, 20),
        { mountedVideoElements: 0, activeMseSessions: 0, bufferedSeconds: 0 },
        [],
        35_000,
      ),
      createMediaSoakSample(
        snapshot(215, 5, 20),
        { mountedVideoElements: 0, activeMseSessions: 0, bufferedSeconds: 0 },
        [],
        125_000,
      ),
    ];

    expect(summarizeMediaSoak(samples, 0)?.idleRecovery).toEqual({
      markedAtMs: 0,
      elapsedSinceMarkSeconds: 125,
      memoryAtMarkBytes: 200,
      memoryAfter30sBytes: 230,
      memoryAfter120sBytes: 215,
      deltaFromBaselineAfter30sBytes: 30,
      deltaFromBaselineAfter120sBytes: 15,
    });
  });

  it("uses Bw, not cold B0, for final RAM and the 30s/120s idle checkpoints", () => {
    const samples = [
      { at: 0, memory: 200 },
      { at: 5_000, memory: 220 },
      { at: 10_000, memory: 300 },
      { at: 40_000, memory: 250 },
      { at: 130_000, memory: 235 },
    ].map(({ at, memory }) => createMediaSoakSample(
      snapshot(memory, 5, 20),
      { mountedVideoElements: 0, activeMseSessions: 0, bufferedSeconds: 0 },
      [],
      at,
    ));

    const summary = summarizeMediaSoak(samples, 10_000, 5_000);
    expect(summary).toMatchObject({
      coldBaselineAppMemoryBytes: 200,
      warmBaselineAppMemoryBytes: 220,
      baselineAppMemoryBytes: 220,
      currentAppMemoryBytes: 235,
      idleRecovery: {
        markedAtMs: 10_000,
        memoryAtMarkBytes: 300,
        memoryAfter30sBytes: 250,
        memoryAfter120sBytes: 235,
        deltaFromBaselineAfter30sBytes: 30,
        deltaFromBaselineAfter120sBytes: 15,
      },
    });
    const evidence = JSON.parse(serializeMediaSoakEvidence(samples, 140_000, 10_000, 5_000));
    expect(evidence.warmBaselineMarkedAtMs).toBe(5_000);
    expect(evidence.summary.warmBaselineAppMemoryBytes).toBe(220);
    expect(evidence.summary.idleRecovery.deltaFromBaselineAfter120sBytes).toBe(15);
  });

  it("refuses expired or future-dated process snapshots instead of refreshing their timestamps", () => {
    const valid = snapshot(200, 10, 40);
    expect(isFreshMediaSoakSnapshot(valid, 1_000, 3_500)).toBe(true);
    expect(isFreshMediaSoakSnapshot(valid, 1_000, 3_501)).toBe(false);
    expect(isFreshMediaSoakSnapshot(valid, 1_000, 999)).toBe(false);
    expect(isFreshMediaSoakSnapshot(valid, null, 1_000)).toBe(false);
    expect(isFreshMediaSoakSnapshot(null, 1_000, 1_000)).toBe(false);
    expect(isFreshMediaSoakSnapshot({ ...valid, sample_ready: false }, 1_000, 1_000)).toBe(false);
  });

  it("records media outages as unavailable instead of zero, and rejects generation-counter discontinuities", () => {
    const frontend = { mountedVideoElements: 0, activeMseSessions: 0, bufferedSeconds: 0 };
    const valid = createMediaSoakSample(snapshot(200, 5, 20), frontend, media(8, 10, 20), 0);
    const missing = createMediaSoakSample(snapshot(230, 5, 20), frontend, null, 40_000);
    const resumed = createMediaSoakSample(snapshot(240, 5, 20), frontend, media(1, 2, 4), 45_000);
    expect(missing.packetBufferBytes).toBeNull();
    expect(missing.generationStarts).toBeNull();
    expect(createMediaSoakSample(snapshot(200, 5, 20), frontend, [], 0).packetBufferBytes).toBe(0);
    const summary = summarizeMediaSoak([valid, missing, resumed]);
    expect(summary).toMatchObject({
      peakPacketBufferBytes: 30,
      generationStartsDelta: null,
      mediaUnavailableSamples: 1,
      maxSampleGapSeconds: 40,
    });
    const evidence = JSON.parse(serializeMediaSoakEvidence([valid, missing, resumed], 50_000, null, null, 2));
    expect(evidence.missedPerformanceSamples).toBe(2);
    expect(evidence.samples[1].packetBufferBytes).toBeNull();
    expect(evidence.summary.generationStartsDelta).toBeNull();
    expect(serializeMediaSoakEvidence([valid, missing, resumed])).not.toContain("front-door");
    expect(summarizeMediaSoak([missing])?.peakPacketBufferBytes).toBeNull();
  });

  it("does not mislabel a late sample as the 30-second recovery measurement", () => {
    const frontend = { mountedVideoElements: 0, activeMseSessions: 0, bufferedSeconds: 0 };
    const samples = [
      createMediaSoakSample(snapshot(200, 5, 20), frontend, [], 0),
      createMediaSoakSample(snapshot(225, 5, 20), frontend, [], 125_000),
    ];
    const recovery = summarizeMediaSoak(samples, 0)?.idleRecovery;
    expect(recovery?.memoryAfter30sBytes).toBeNull();
    expect(recovery?.deltaFromBaselineAfter30sBytes).toBeNull();
    expect(recovery?.memoryAfter120sBytes).toBe(225);
    expect(summarizeMediaSoak(samples)?.maxSampleGapSeconds).toBe(125);
  });

  it("stops at the two-hour budget without shifting the cold baseline", () => {
    let samples = [] as ReturnType<typeof createMediaSoakSample>[];
    for (let index = 0; index < MEDIA_SOAK_MAX_SAMPLES + 5; index += 1) {
      samples = appendMediaSoakSample(
        samples,
        createMediaSoakSample(
          snapshot(index + 1, 1, 0),
          { mountedVideoElements: 0, activeMseSessions: 0, bufferedSeconds: 0 },
          [],
          index * 5_000,
        ),
      );
    }

    expect(samples).toHaveLength(MEDIA_SOAK_MAX_SAMPLES);
    expect(samples[0]?.appMemoryBytes).toBe(1);
    expect(samples.at(-1)?.appMemoryBytes).toBe(MEDIA_SOAK_MAX_SAMPLES);
  });
});
