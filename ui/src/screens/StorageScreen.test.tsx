import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { StorageScreen } from "./StorageScreen";
import type { ApplicationSettings } from "../lib/tauri";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

const GIB = 1024 ** 3;
const initial: ApplicationSettings = {
  storage_root: "/var/lib/nian-vision/recordings",
  segment_target_secs: 300,
  max_age_days: null,
  max_storage_bytes: null,
  cleanup_target_bytes: null,
  launch_at_login: false,
};

function installDesktop() {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "settings_get") return initial;
    if (command === "settings_update") return (args as { settings: ApplicationSettings }).settings;
    throw new Error(`unexpected command ${command}`);
  });
}

beforeEach(() => {
  vi.mocked(invoke).mockReset();
  vi.mocked(open).mockReset();
});

afterEach(() => {
  cleanup();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("StorageScreen", () => {
  it("requires HIGH and LOW watermarks to be configured together", async () => {
    installDesktop();
    render(<StorageScreen />);
    await screen.findByDisplayValue(initial.storage_root!);
    fireEvent.change(screen.getByLabelText("Storage high watermark GB"), { target: { value: "10" } });
    fireEvent.click(screen.getByRole("button", { name: "Save storage settings" }));

    expect(screen.getByRole("alert").textContent).toContain("must be configured together");
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "settings_update")).toBe(false);
  });

  it("requires LOW watermark to be below HIGH watermark", async () => {
    installDesktop();
    render(<StorageScreen />);
    await screen.findByDisplayValue(initial.storage_root!);
    fireEvent.change(screen.getByLabelText("Storage high watermark GB"), { target: { value: "10" } });
    fireEvent.change(screen.getByLabelText("Cleanup low watermark GB"), { target: { value: "10" } });
    fireEvent.click(screen.getByRole("button", { name: "Save storage settings" }));

    expect(screen.getByRole("alert").textContent).toContain("must be lower than the storage high watermark");
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "settings_update")).toBe(false);
  });

  it("converts friendly GiB inputs back to authoritative byte settings", async () => {
    installDesktop();
    render(<StorageScreen />);
    await screen.findByDisplayValue(initial.storage_root!);
    fireEvent.change(screen.getByLabelText("Storage high watermark GB"), { target: { value: "10" } });
    fireEvent.change(screen.getByLabelText("Cleanup low watermark GB"), { target: { value: "8" } });
    fireEvent.click(screen.getByRole("button", { name: "Save storage settings" }));

    await screen.findByText("Storage settings saved.");
    const call = vi.mocked(invoke).mock.calls.find(([name]) => name === "settings_update");
    expect(call?.[1]).toEqual({
      settings: {
        ...initial,
        max_storage_bytes: 10 * GIB,
        cleanup_target_bytes: 8 * GIB,
      },
    });
    await waitFor(() => expect(screen.getByDisplayValue("8")).toBeTruthy());
  });

  it("uses the native directory picker for the recordings root", async () => {
    installDesktop();
    vi.mocked(open).mockResolvedValue("E:\\Camera");
    render(<StorageScreen />);
    await screen.findByDisplayValue(initial.storage_root!);

    fireEvent.click(screen.getByRole("button", { name: "Browse folder" }));

    await waitFor(() => expect(screen.getByDisplayValue("E:\\Camera")).toBeTruthy());
    expect(open).toHaveBeenCalledWith(expect.objectContaining({ directory: true, multiple: false }));
  });
});
