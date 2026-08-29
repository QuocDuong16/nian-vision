import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { App } from "./App";

afterEach(cleanup);

describe("App shell", () => {
  it("renders navigation and honest empty states", () => {
    render(<App />);

    for (const label of ["Cameras", "Timeline", "Storage", "Settings"]) {
      expect(screen.getByRole("button", { name: label })).toBeTruthy();
    }

    expect(screen.getByText("No cameras configured")).toBeTruthy();
    expect(screen.getByText(/Add an RTSP camera/)).toBeTruthy();
  });

  it("switches screens when navigation is clicked", () => {
    render(<App />);

    fireEvent.click(screen.getByRole("button", { name: "Timeline" }));
    expect(screen.getByText("No recordings yet")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(
      screen.getByRole("heading", { name: "Application settings" })
    ).toBeTruthy();
  });
});
