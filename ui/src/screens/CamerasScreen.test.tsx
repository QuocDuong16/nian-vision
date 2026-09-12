import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CamerasScreen } from "./CamerasScreen";
import type { CameraSummary, EventStatus, RecordingStatus } from "../lib/tauri";

const tauriWindowMock = vi.hoisted(() => ({
  closeHandler: undefined as undefined | (() => void),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onCloseRequested: async (handler: () => void) => {
      tauriWindowMock.closeHandler = handler;
      return () => {
        if (tauriWindowMock.closeHandler === handler) tauriWindowMock.closeHandler = undefined;
      };
    },
  }),
}));

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

const onvifDiscovery = {
  session_id: "session-1",
  devices: [
    {
      device_id: "device-1",
      endpoint_reference: "urn:uuid:front-onvif",
      label: "Front ONVIF",
      network_address: "192.168.1.80",
    },
  ],
};

const onvifConnection = {
  session_id: "session-1",
  device_id: "device-1",
  manufacturer: "Fixture Corp",
  model: "Fixture Cam",
  firmware_version: "1.0",
  serial_number: "fixture-001",
  hostname: "front-onvif",
  profiles: [
    {
      token: "main",
      name: "Main H264",
      video_codec: "H264",
      width: 1920,
      height: 1080,
      framerate: 25,
      bitrate_kbps: 4096,
      audio_codec: "AAC",
      supported: true,
      recommended: true,
    },
    {
      token: "hevc",
      name: "Main H265",
      video_codec: "H265",
      width: 3840,
      height: 2160,
      framerate: 25,
      bitrate_kbps: 8192,
      audio_codec: null,
      supported: false,
      recommended: false,
    },
  ],
  proposed_camera_id: "onvif-front",
  proposed_display_name: "Front provisioned",
};

const onvifPrepared = {
  session_id: "session-1",
  device_id: "device-1",
  profile_token: "main",
  host: "192.168.1.80",
  port: 8554,
  path: "/live/main",
  host_mismatch: false,
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
  fireEvent.click(screen.getByRole("button", { name: "Add RTSP manually" }));
  fireEvent.change(screen.getByLabelText("Display name"), { target: { value: "Back yard" } });
  fireEvent.change(screen.getByLabelText("Host / IP"), { target: { value: "192.168.1.51" } });
  fireEvent.change(screen.getByLabelText("Username"), { target: { value: "admin" } });
  fireEvent.change(screen.getByLabelText("Password"), { target: { value: "SENTINEL-ui-password" } });
}

beforeEach(() => {
  vi.mocked(invoke).mockReset();
  tauriWindowMock.closeHandler = undefined;
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
    fireEvent.click(screen.getByRole("button", { name: "Add RTSP manually" }));
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

  it("blocks Start while Desired is Off but runtime remains active, then re-enables after convergence", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      value: {},
      configurable: true,
    });
    let desiredCameras = [camera.camera_id];
    let runtime: RecordingStatus[] = [{ ...stopped, state: "recording", camera_id: camera.camera_id }];
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera, backCamera];
      if (command === "recording_statuses") return runtime;
      if (command === "recording_intent") return { camera_ids: desiredCameras };
      if (command === "recording_start") throw new Error("recording_start must not be admitted during convergence");
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("Front door");
    const frontCard = screen.getByText("Front door").closest("article") as HTMLElement;
    const backCard = screen.getByText("Back door").closest("article") as HTMLElement;
    expect((within(frontCard).getByRole("button", { name: "Stop" }) as HTMLButtonElement).disabled).toBe(false);
    expect((within(backCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);
    expect(screen.getByText("Active 1 camera")).toBeTruthy();

    desiredCameras = [];
    await waitFor(
      () => {
        const stopping = within(frontCard).getByRole("button", { name: "Stopping…" }) as HTMLButtonElement;
        expect(stopping.disabled).toBe(true);
      },
      { timeout: 1_500 },
    );
    expect((within(backCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);
    expect(vi.mocked(invoke).mock.calls.filter(([name]) => name === "recording_start")).toHaveLength(0);

    runtime = [{ ...stopped, camera_id: camera.camera_id }];
    await waitFor(
      () => {
        const start = within(frontCard).getByRole("button", { name: "Start" }) as HTMLButtonElement;
        expect(start.disabled).toBe(false);
      },
      { timeout: 1_500 },
    );
    expect((within(backCard).getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);
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

  it("shows an empty ONVIF discovery result without adopting a camera", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "onvif_discover") return { session_id: "empty-session", devices: [] };
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("No cameras configured");
    fireEvent.click(screen.getByRole("button", { name: "Add camera" }));
    fireEvent.click(screen.getByRole("button", { name: "Discover ONVIF cameras" }));

    expect(await screen.findByText("No ONVIF cameras found.")).toBeTruthy();
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "camera_create")).toBe(false);
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "onvif_add_camera")).toBe(false);
  });

  it("clears the submitted ONVIF password after an authentication failure", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "onvif_discover") return onvifDiscovery;
      if (command === "onvif_connect") {
        throw { code: "onvif_auth_failed", message: "ONVIF authentication failed" };
      }
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("No cameras configured");
    fireEvent.click(screen.getByRole("button", { name: "Add camera" }));
    fireEvent.click(screen.getByRole("button", { name: "Discover ONVIF cameras" }));
    await screen.findByText("Front ONVIF");
    fireEvent.click(screen.getByRole("button", { name: "Select" }));
    fireEvent.change(screen.getByLabelText("ONVIF username"), { target: { value: "admin" } });
    fireEvent.change(screen.getByLabelText("ONVIF password"), { target: { value: "SENTINEL-onvif-password" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("ONVIF authentication failed"));
    expect((screen.getByLabelText("ONVIF password") as HTMLInputElement).value).toBe("");
    expect(document.body.textContent).not.toContain("SENTINEL-onvif-password");
  });

  it("clears ONVIF wizard state and typed credentials on a native close request", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "onvif_discover") return onvifDiscovery;
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("No cameras configured");
    await waitFor(() => expect(tauriWindowMock.closeHandler).toBeTypeOf("function"));
    fireEvent.click(screen.getByRole("button", { name: "Add camera" }));
    fireEvent.click(screen.getByRole("button", { name: "Discover ONVIF cameras" }));
    await screen.findByText("Front ONVIF");
    fireEvent.click(screen.getByRole("button", { name: "Select" }));
    fireEvent.change(screen.getByLabelText("ONVIF username"), { target: { value: "admin" } });
    fireEvent.change(screen.getByLabelText("ONVIF password"), {
      target: { value: "SENTINEL-close-password" },
    });

    act(() => tauriWindowMock.closeHandler?.());

    await waitFor(() => expect(screen.queryByLabelText("ONVIF password")).toBeNull());
    expect(screen.queryByText("Front ONVIF")).toBeNull();
    expect(document.body.textContent).not.toContain("SENTINEL-close-password");
  });


  it("provisions an H264 ONVIF profile without exposing credentials or an authenticated RTSP URI", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    let saved = false;
    let addArgs: unknown;
    const provisioned: CameraSummary = {
      camera_id: "onvif-front",
      display_name: "Front provisioned",
      host: "192.168.1.80",
      port: 8554,
      path: "/live/main",
      audio_policy: "copy_all",
    };
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return saved ? [camera, provisioned] : [camera];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "onvif_discover") return onvifDiscovery;
      if (command === "onvif_connect") return onvifConnection;
      if (command === "onvif_prepare_profile") return onvifPrepared;
      if (command === "onvif_add_camera") {
        addArgs = args;
        saved = true;
        return { value: provisioned, warning: null };
      }
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("Front door");
    expect((screen.getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Add camera" }));
    fireEvent.click(screen.getByRole("button", { name: "Discover ONVIF cameras" }));
    await screen.findByText("Front ONVIF");
    fireEvent.click(screen.getByRole("button", { name: "Select" }));
    fireEvent.change(screen.getByLabelText("ONVIF username"), { target: { value: "admin" } });
    fireEvent.change(screen.getByLabelText("ONVIF password"), { target: { value: "SENTINEL-onvif-password" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));

    const h264 = await screen.findByText("Main H264");
    const h264Card = h264.closest("article") as HTMLElement;
    const h265Card = screen.getByText("Main H265").closest("article") as HTMLElement;
    expect((within(h265Card).getByRole("button", { name: "Use profile" }) as HTMLButtonElement).disabled).toBe(true);
    expect(document.body.textContent).not.toContain("SENTINEL-onvif-password");
    fireEvent.click(within(h264Card).getByRole("button", { name: "Use profile" }));

    expect(await screen.findByText("192.168.1.80:8554/live/main")).toBeTruthy();
    expect(document.body.textContent).not.toContain("rtsp://");
    expect(document.body.textContent).not.toContain("SENTINEL-onvif-password");
    fireEvent.click(screen.getByRole("button", { name: "Test & Add" }));

    await screen.findByText("Front provisioned");
    const serialized = JSON.stringify(addArgs);
    expect(serialized).not.toContain("username");
    expect(serialized).not.toContain("password");
    expect(serialized).not.toContain("rtsp://");
    expect(serialized).toContain("profile_token");
    expect((screen.getAllByRole("button", { name: "Start" })[0] as HTMLButtonElement).disabled).toBe(false);
  });

  it("keeps the ONVIF review open when final provisioning fails", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "onvif_discover") return onvifDiscovery;
      if (command === "onvif_connect") return onvifConnection;
      if (command === "onvif_prepare_profile") return onvifPrepared;
      if (command === "onvif_add_camera") {
        throw { code: "source_open_failed", message: "camera source could not be opened" };
      }
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("No cameras configured");
    fireEvent.click(screen.getByRole("button", { name: "Add camera" }));
    fireEvent.click(screen.getByRole("button", { name: "Discover ONVIF cameras" }));
    await screen.findByText("Front ONVIF");
    fireEvent.click(screen.getByRole("button", { name: "Select" }));
    fireEvent.change(screen.getByLabelText("ONVIF username"), { target: { value: "admin" } });
    fireEvent.change(screen.getByLabelText("ONVIF password"), { target: { value: "secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    const h264Card = (await screen.findByText("Main H264")).closest("article") as HTMLElement;
    fireEvent.click(within(h264Card).getByRole("button", { name: "Use profile" }));
    await screen.findByText("192.168.1.80:8554/live/main");
    fireEvent.click(screen.getByRole("button", { name: "Test & Add" }));

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("camera source could not be opened"));
    expect(screen.getByRole("button", { name: "Test & Add" })).toBeTruthy();
  });

  it("refreshes discovery into a new session and removes stale device choices", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    let discoveryCount = 0;
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "onvif_discover") {
        discoveryCount += 1;
        if (discoveryCount === 1) return onvifDiscovery;
        return {
          session_id: "session-2",
          devices: [{
            device_id: "device-2",
            endpoint_reference: "urn:uuid:garage-onvif",
            label: "Garage ONVIF",
            network_address: "192.168.1.81",
          }],
        };
      }
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("No cameras configured");
    fireEvent.click(screen.getByRole("button", { name: "Add camera" }));
    fireEvent.click(screen.getByRole("button", { name: "Discover ONVIF cameras" }));
    await screen.findByText("Front ONVIF");
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));

    expect(await screen.findByText("Garage ONVIF")).toBeTruthy();
    expect(screen.queryByText("Front ONVIF")).toBeNull();
  });

  it("pairs PTZ through explicit ONVIF selection without forwarding credentials to ptz_pair", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    let paired = false;
    let pairArgs: unknown;
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "ptz_configured") return paired;
      if (command === "onvif_discover") return onvifDiscovery;
      if (command === "onvif_connect") return onvifConnection;
      if (command === "ptz_pair") {
        pairArgs = args;
        paired = true;
        return {
          value: {
            camera_id: camera.camera_id,
            configured: true,
            ptz_supported: true,
            pan_tilt_supported: true,
            zoom_supported: true,
            state: "ready",
            error: null,
          },
          warning: null,
        };
      }
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("Front door");
    fireEvent.click(screen.getByRole("button", { name: "Pair PTZ" }));
    expect(await screen.findByRole("dialog", { name: "ONVIF PTZ pairing" })).toBeTruthy();
    await screen.findByText("Front ONVIF");
    fireEvent.click(screen.getByRole("button", { name: "Select" }));
    fireEvent.change(screen.getByLabelText("ONVIF username"), { target: { value: "admin" } });
    fireEvent.change(screen.getByLabelText("ONVIF password"), {
      target: { value: "SENTINEL-ptz-password" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Authenticate & Pair" }));

    expect(await screen.findByRole("button", { name: "Replace PTZ" })).toBeTruthy();
    const serialized = JSON.stringify(pairArgs);
    expect(serialized).toContain(camera.camera_id);
    expect(serialized).toContain("session-1");
    expect(serialized).toContain("device-1");
    expect(serialized).not.toContain("username");
    expect(serialized).not.toContain("password");
    expect(serialized).not.toContain("SENTINEL-ptz-password");
    expect(document.body.textContent).not.toContain("SENTINEL-ptz-password");
  });

  it("pairs Motion Events without forwarding credentials and keeps Desired Off until explicitly enabled", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    let status: EventStatus = {
      camera_id: camera.camera_id, configured: false, desired: false, state: "disabled",
      motion_active: null, last_event_at: null, last_error_code: null,
    };
    let pairArgs: unknown;
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "ptz_configured") return false;
      if (command === "event_statuses") return [status];
      if (command === "event_pair_saved") {
        pairArgs = args;
        status = { ...status, configured: true };
        return { value: status, warning: null };
      }
      if (command === "event_enable") {
        status = { ...status, desired: true, state: "polling" as const };
        return status;
      }
      if (command === "event_disable") {
        status = { ...status, desired: false, state: "disabled" as const };
        return status;
      }
      if (command === "event_unpair") {
        status = { ...status, configured: false, desired: false, state: "disabled" as const };
        return { value: status, warning: null };
      }
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("Front door");
    fireEvent.click(screen.getByRole("button", { name: "Pair Motion Events" }));
    expect(await screen.findByRole("button", { name: "Replace Motion Events" })).toBeTruthy();
    expect(screen.queryByRole("dialog", { name: "ONVIF motion event pairing" })).toBeNull();
    expect(screen.getByRole("button", { name: "Enable Events" })).toBeTruthy();
    expect(screen.getByText(/Motion Events: Off/)).toBeTruthy();
    expect(pairArgs).toEqual({ cameraId: camera.camera_id });

    fireEvent.click(screen.getByRole("button", { name: "Enable Events" }));
    expect(await screen.findByRole("button", { name: "Disable Events" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Unpair Events" }));
    expect(await screen.findByRole("button", { name: "Pair Motion Events" })).toBeTruthy();
  });

  it("falls back to explicit ONVIF credentials only when the saved camera credential is rejected", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "ptz_configured") return false;
      if (command === "event_statuses") return [{
        camera_id: camera.camera_id, configured: false, desired: false, state: "disabled",
        motion_active: null, last_event_at: null, last_error_code: null,
      }];
      if (command === "event_pair_saved") {
        throw { code: "onvif_auth_failed", message: "ONVIF authentication failed" };
      }
      if (command === "onvif_discover") return onvifDiscovery;
      if (command === "onvif_cancel") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("Front door");
    fireEvent.click(screen.getByRole("button", { name: "Pair Motion Events" }));

    expect(await screen.findByRole("dialog", { name: "ONVIF motion event pairing" })).toBeTruthy();
    expect(await screen.findByText("Front ONVIF")).toBeTruthy();
  });

  it("unpairs PTZ without deleting or mutating the RTSP camera row", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    let paired = true;
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_statuses") return [];
      if (command === "recording_intent") return { camera_ids: [] };
      if (command === "ptz_configured") return paired;
      if (command === "ptz_unpair") {
        expect(args).toEqual({ cameraId: camera.camera_id });
        paired = false;
        return {
          value: {
            camera_id: camera.camera_id,
            configured: false,
            ptz_supported: false,
            pan_tilt_supported: false,
            zoom_supported: false,
            state: null,
            error: null,
          },
          warning: null,
        };
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<CamerasScreen />);
    await screen.findByText("Front door");
    expect(await screen.findByRole("button", { name: "Unpair PTZ" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Unpair PTZ" }));
    expect(await screen.findByRole("button", { name: "Pair PTZ" })).toBeTruthy();
    expect(screen.getByText("192.168.1.50:554/stream1")).toBeTruthy();
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "camera_delete")).toBe(false);
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "camera_update")).toBe(false);
  });

});
