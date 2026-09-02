import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CamerasScreen } from "./CamerasScreen";
import type { CameraSummary, RecordingStatus } from "../lib/tauri";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const camera: CameraSummary = {
  camera_id: "front-door",
  display_name: "Front door",
  host: "192.168.1.50",
  port: 554,
  path: "/stream1",
  audio_policy: "copy_all",
};

const backCamera: CameraSummary = {
  camera_id: "back-door",
  display_name: "Back door",
  host: "192.168.1.51",
  port: 554,
  path: "/stream1",
  audio_policy: "copy_all",
};

const sideCamera: CameraSummary = {
  camera_id: "side-door",
  display_name: "Side door",
  host: "192.168.1.52",
  port: 554,
  path: "/stream1",
  audio_policy: "copy_all",
};

const stopped: RecordingStatus = {
  state: "stopped",
  camera_id: null,
  failure_category: null,
  reconnect_attempt: 0,
  finalized_segments: 0,
};

function installDesktop(cameras: CameraSummary[] = [], status: RecordingStatus = stopped) {
  Object.defineProperty(window, "__TAURI_INTERNALS__", {
    value: {},
    configurable: true,
  });
  vi.mocked(invoke).mockImplementation(async (command) => {
    if (command === "camera_list") return cameras;
    if (command === "recording_statuses") return status.camera_id ? [status] : [];
    if (command === "recording_intent") return { camera_ids: status.camera_id ? [status.camera_id] : [] };
    throw new Error(`unexpected command ${command}`);
  });
}

function fillNewCamera() {
  fireEvent.click(screen.getByRole("button", { name: "Add camera" }));
  fireEvent.change(screen.getByLabelText("Display name"), { target: { value: "Back yard" } });
  fireEvent.change(screen.getByLabelText("Host / IP"), { target: { value: "192.168.1.51" } });
  fireEvent.change(screen.getByLabelText("Username"), { target: { value: "admin" } });
  fireEvent.change(screen.getByLabelText("Password"), { target: { value: "SENTINEL-ui-password" } });
}

beforeEach(() => {
  vi.mocked(invoke).mockReset();
});

afterEach(() => {
  cleanup();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("CamerasScreen", () => {
  it("renders the camera empty state", async () => {
    installDesktop();
    render(<CamerasScreen />);
    expect(await screen.findByText("No cameras configured")).toBeTruthy();
  });

  it("validates the create form before invoking the backend", async () => {
    installDesktop();
    render(<CamerasScreen />);
    await screen.findByText("No cameras configured");
    fireEvent.click(screen.getByRole("button", { name: "Add camera" }));
    fireEvent.click(screen.getByRole("button", { name: "Save camera" }));
    expect(screen.getByRole("alert").textContent).toContain("Display name is required");
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "camera_create")).toBe(false);
  });

  it("renders saved cameras without ever rendering a password", async () => {
    installDesktop([camera]);
    render(<CamerasScreen />);
    expect(await screen.findByText("Front door")).toBeTruthy();
    expect(screen.getByText("192.168.1.50:554/stream1")).toBeTruthy();
    expect(document.body.textContent).not.toContain("SENTINEL-ui-password");
  });

  it("tests an unsaved connection with loading and typed result state", async () => {
    installDesktop();
    let resolveProbe: ((value: unknown) => void) | undefined;
    vi.mocked(invoke).mockImplementation((command) => {
      if (command === "camera_list") return Promise.resolve([]);
      if (command === "recording_statuses") return Promise.resolve([]);
      if (command === "recording_intent") return Promise.resolve({ camera_ids: [] });
      if (command === "camera_probe") {
        return new Promise((resolve) => { resolveProbe = resolve; });
      }
      throw new Error(`unexpected command ${command}`);
    });
    render(<CamerasScreen />);
    await screen.findByText("No cameras configured");
    fillNewCamera();
    fireEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(screen.getByRole("button", { name: "Testing…" })).toBeTruthy();
    resolveProbe?.({
      reachable: true,
      video_stream_found: true,
      codec: "h264",
      width: 1920,
      height: 1080,
      audio_stream_count: 1,
    });
    await waitFor(() => expect(screen.getByRole("status").textContent).toContain("Video h264"));
    const probeCall = vi.mocked(invoke).mock.calls.find(([name]) => name === "camera_probe");
    expect(JSON.stringify(probeCall?.[1])).toContain("SENTINEL-ui-password");
  });

  it("uses backend recording state for Start and Stop transitions", async () => {
    installDesktop([camera]);
    let desiredCameras: string[] = [];
    let runtime: RecordingStatus[] = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_statuses") return runtime;
      if (command === "recording_intent") return { camera_ids: desiredCameras };
      if (command === "recording_start") {
        expect(args).toEqual({ cameraId: camera.camera_id });
        desiredCameras = [camera.camera_id];
        runtime = [{ ...stopped, state: "starting", camera_id: camera.camera_id }];
        return runtime[0];
      }
      if (command === "recording_stop") {
        expect(args).toEqual({ cameraId: camera.camera_id });
        desiredCameras = [];
        runtime = [{ ...stopped, state: "stopping", camera_id: camera.camera_id }];
        return runtime[0];
      }
      throw new Error(`unexpected command ${command}`);
    });
    render(<CamerasScreen />);
    await screen.findByText("Front door");
    fireEvent.click(screen.getByRole("button", { name: "Start" }));
    await screen.findByRole("button", { name: "Stop" });
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    await waitFor(() => expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "recording_stop")).toBe(true));
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "recording_start")).toBe(true);
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "recording_stop")).toBe(true);
  });

  it("tracks concurrent recording operations independently per camera", async () => {
    installDesktop([camera, backCamera, sideCamera]);
    let desiredCameras: string[] = [];
    let runtime: RecordingStatus[] = [];
    const resolvers = new Map<string, (value: RecordingStatus) => void>();
    vi.mocked(invoke).mockImplementation((command, args) => {
      if (command === "camera_list") return Promise.resolve([camera, backCamera, sideCamera]);
      if (command === "recording_statuses") return Promise.resolve(runtime);
      if (command === "recording_intent") return Promise.resolve({ camera_ids: desiredCameras });
      if (command === "recording_start") {
        const cameraId = (args as { cameraId: string }).cameraId;
        return new Promise((resolve) => {
          resolvers.set(cameraId, resolve);
        });
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("Front door");
    const frontCard = screen.getByText("Front door").closest("article") as HTMLElement;
    const backCard = screen.getByText("Back door").closest("article") as HTMLElement;
    const sideCard = screen.getByText("Side door").closest("article") as HTMLElement;

    fireEvent.click(within(frontCard).getByRole("button", { name: "Start" }));
    await waitFor(() => {
      expect((within(frontCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(true);
    });
    expect((within(backCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);
    expect((within(backCard).getByRole("button", { name: "Edit" }) as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(within(backCard).getByRole("button", { name: "Start" }));
    await waitFor(() => {
      expect((within(frontCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(true);
      expect((within(backCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(true);
    });
    expect((within(sideCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);

    const frontStarting: RecordingStatus = { ...stopped, state: "starting", camera_id: camera.camera_id };
    desiredCameras = [camera.camera_id];
    runtime = [frontStarting];
    resolvers.get(camera.camera_id)?.(frontStarting);
    await waitFor(() => {
      expect((within(frontCard).getByRole("button", { name: "Stop" }) as HTMLButtonElement).disabled).toBe(false);
    });
    expect((within(backCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(true);
    expect((within(sideCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);

    const backStarting: RecordingStatus = { ...stopped, state: "starting", camera_id: backCamera.camera_id };
    desiredCameras = [camera.camera_id, backCamera.camera_id];
    runtime = [frontStarting, backStarting];
    resolvers.get(backCamera.camera_id)?.(backStarting);
    await waitFor(() => {
      expect((within(backCard).getByRole("button", { name: "Stop" }) as HTMLButtonElement).disabled).toBe(false);
    });
    expect(vi.mocked(invoke).mock.calls.filter(([name]) => name === "recording_start")).toHaveLength(2);
  });

  it("admits only one same-camera recording command while the first is pending", async () => {
    installDesktop([camera]);
    vi.mocked(invoke).mockImplementation((command) => {
      if (command === "camera_list") return Promise.resolve([camera]);
      if (command === "recording_statuses") return Promise.resolve([]);
      if (command === "recording_intent") return Promise.resolve({ camera_ids: [] });
      if (command === "recording_start") return new Promise(() => {});
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("Front door");
    const start = screen.getByRole("button", { name: "Start" });
    fireEvent.click(start);
    fireEvent.click(start);

    expect(vi.mocked(invoke).mock.calls.filter(([name]) => name === "recording_start")).toHaveLength(1);
  });

  it("displays backend errors instead of optimistic success", async () => {
    installDesktop([camera]);
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "recording_start") throw { code: "storage_failed", message: "recording storage is not configured" };
      throw new Error(`unexpected command ${command}`);
    });
    render(<CamerasScreen />);
    await screen.findByText("Front door");
    fireEvent.click(screen.getByRole("button", { name: "Start" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("recording storage is not configured"));
  });

  it("requires delete confirmation and explains that footage survives", async () => {
    installDesktop([camera]);
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "camera_delete") return { value: camera, warning: null };
      throw new Error(`unexpected command ${command}`);
    });
    render(<CamerasScreen />);
    await screen.findByText("Front door");
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = screen.getByRole("dialog", { name: "Delete camera confirmation" });
    expect(dialog.textContent).toContain("Existing footage stays on disk");
    fireEvent.click(screen.getByRole("button", { name: "Delete camera" }));
    await waitFor(() => expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "camera_delete")).toBe(true));
  });

  it("locks recording-critical edit fields and delete while a camera is active", async () => {
    const recording: RecordingStatus = {
      state: "recording",
      camera_id: camera.camera_id,
      failure_category: null,
      reconnect_attempt: 0,
      finalized_segments: 4,
    };
    installDesktop([camera], recording);
    render(<CamerasScreen />);
    await screen.findByText("Front door");
    const deleteButton = screen.getByRole("button", { name: "Delete" }) as HTMLButtonElement;
    expect(deleteButton.disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Edit" }));
    expect((screen.getByLabelText("Display name") as HTMLInputElement).disabled).toBe(false);
    expect((screen.getByLabelText("Host / IP") as HTMLInputElement).disabled).toBe(true);
    expect((screen.getByLabelText("Password") as HTMLInputElement).disabled).toBe(true);
    expect(screen.getByText(/Endpoint, credentials and audio policy are locked/)).toBeTruthy();
  });
});
