import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { App, isDesktopReloadShortcut } from "./App";

afterEach(cleanup);

describe("App shell", () => {
  it("recognizes desktop reload shortcuts that must not escape into WebView navigation", () => {
    expect(isDesktopReloadShortcut({ key: "F5", ctrlKey: false, metaKey: false })).toBe(true);
    expect(isDesktopReloadShortcut({ key: "r", ctrlKey: true, metaKey: false })).toBe(true);
    expect(isDesktopReloadShortcut({ key: "R", ctrlKey: true, metaKey: false })).toBe(true);
    expect(isDesktopReloadShortcut({ key: "r", ctrlKey: false, metaKey: true })).toBe(true);
    expect(isDesktopReloadShortcut({ key: "r", ctrlKey: false, metaKey: false })).toBe(false);
  });

  it("renders navigation and honest empty states", () => {
    render(<App />);

    for (const label of ["Cameras", "Live View", "Timeline", "Storage", "Settings"]) {
      expect(screen.getByRole("button", { name: label })).toBeTruthy();
    }

    expect(screen.getByText("No cameras configured")).toBeTruthy();
    expect(screen.getByText(/Add an RTSP camera/)).toBeTruthy();
  });

  it("keeps the live workspace mounted after the first visit", () => {
    render(<App />);

    fireEvent.click(screen.getByRole("button", { name: "Live View" }));
    const liveHeading = screen.getByRole("heading", { name: "Live View" });
    const resident = liveHeading.closest('[data-resident-screen="live"]');
    expect(resident).not.toBeNull();
    expect(resident?.hasAttribute("hidden")).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "Cameras" }));
    expect(liveHeading.isConnected).toBe(true);
    expect(resident?.hasAttribute("hidden")).toBe(true);

    fireEvent.click(screen.getByRole("button", { name: "Live View" }));
    expect(screen.getByRole("heading", { name: "Live View" })).toBe(liveHeading);
  });

  it("switches screens when navigation is clicked", () => {
    render(<App />);

    fireEvent.click(screen.getByRole("button", { name: "Timeline" }));
    expect(screen.getByText("Recordings timeline")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(
      screen.getByRole("heading", { name: "Application settings" })
    ).toBeTruthy();
  });
});
