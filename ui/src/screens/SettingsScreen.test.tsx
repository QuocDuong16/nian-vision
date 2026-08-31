import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SettingsScreen } from "./SettingsScreen";
import type { ApplicationSettings, UpdateCheck } from "../lib/tauri";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const settings: ApplicationSettings = {
  storage_root: null,
  segment_target_secs: 60,
  max_age_days: null,
  max_storage_bytes: null,
  cleanup_target_bytes: null,
  launch_at_login: false,
};

function installDesktop(update: UpdateCheck) {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(invoke).mockImplementation(async (command) => {
    if (command === "settings_get") return settings;
    if (command === "app_info") return { name: "Nian Vision", version: "0.1.0" };
    if (command === "update_check") return update;
    if (command === "update_install") return undefined;
    throw new Error(`unexpected command ${command}`);
  });
}

beforeEach(() => {
  vi.mocked(invoke).mockReset();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("SettingsScreen updates", () => {
  it("shows the current version and reports an unconfigured update channel", async () => {
    installDesktop({ configured: false, current_version: "0.1.0", available: null });
    render(<SettingsScreen />);

    expect((await screen.findByText(/Current version:/)).textContent).toContain("0.1.0");
    fireEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    expect(await screen.findByText(/no production update channel configured/i)).toBeTruthy();
  });

  it("requires explicit confirmation before update installation", async () => {
    installDesktop({
      configured: true,
      current_version: "0.1.0",
      available: { version: "0.2.0", notes: "Release notes", date: null },
    });
    vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<SettingsScreen />);
    await screen.findByText(/Current version:/);

    fireEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    const install = await screen.findByRole("button", { name: "Download and install 0.2.0" });
    fireEvent.click(install);

    expect(window.confirm).toHaveBeenCalledOnce();
    expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "update_install")).toBe(false);
  });

  it("passes the checked version to the Rust lifecycle install command", async () => {
    installDesktop({
      configured: true,
      current_version: "0.1.0",
      available: { version: "0.2.0", notes: null, date: "2026-08-31T00:00:00Z" },
    });
    vi.spyOn(window, "confirm").mockReturnValue(true);
    render(<SettingsScreen />);
    await screen.findByText(/Current version:/);

    fireEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    fireEvent.click(await screen.findByRole("button", { name: "Download and install 0.2.0" }));

    await waitFor(() => {
      expect(vi.mocked(invoke)).toHaveBeenCalledWith("update_install", { expectedVersion: "0.2.0" });
    });
  });
});
