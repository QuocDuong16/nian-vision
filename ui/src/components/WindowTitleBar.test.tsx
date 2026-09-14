import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WindowTitleBar } from "./WindowTitleBar";

const minimize = vi.fn().mockResolvedValue(undefined);
const toggleMaximize = vi.fn().mockResolvedValue(undefined);
const close = vi.fn().mockResolvedValue(undefined);

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ minimize, toggleMaximize, close }),
}));

beforeEach(() => {
  minimize.mockClear();
  toggleMaximize.mockClear();
  close.mockClear();
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
});

afterEach(() => {
  cleanup();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("WindowTitleBar", () => {
  it("exposes application-owned window controls", () => {
    render(<WindowTitleBar title="Nian Vision" />);
    expect(screen.getByText("Nian Vision")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Minimize window" }));
    fireEvent.click(screen.getByRole("button", { name: "Maximize or restore window" }));
    fireEvent.click(screen.getByRole("button", { name: "Close window" }));

    expect(minimize).toHaveBeenCalledTimes(1);
    expect(toggleMaximize).toHaveBeenCalledTimes(1);
    expect(close).toHaveBeenCalledTimes(1);
  });

  it("marks the titlebar as a Tauri drag region", () => {
    const view = render(<WindowTitleBar />);
    const titlebar = view.container.querySelector(".window-titlebar");
    expect(titlebar?.hasAttribute("data-tauri-drag-region")).toBe(true);
  });
});
