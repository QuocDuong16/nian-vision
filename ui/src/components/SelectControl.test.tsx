import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { SelectControl } from "./SelectControl";

const options = [
  { value: "main", label: "Main stream", description: "1920 × 1080" },
  { value: "sub", label: "Sub stream", description: "640 × 360" },
];

describe("SelectControl", () => {
  it("selects an option from the portal listbox", () => {
    const onChange = vi.fn();
    render(
      <SelectControl
        ariaLabel="Stream profile"
        value="main"
        options={options}
        onChange={onChange}
      />,
    );

    const trigger = screen.getByRole("combobox", { name: "Stream profile" });
    expect(trigger.textContent).toContain("Main stream");

    fireEvent.click(trigger);
    expect(screen.getByRole("listbox", { name: "Stream profile options" })).toBeTruthy();
    fireEvent.click(screen.getByRole("option", { name: /Sub stream/ }));

    expect(onChange).toHaveBeenCalledWith("sub");
    expect(screen.queryByRole("listbox", { name: "Stream profile options" })).toBeNull();
  });

  it("opens from the keyboard and closes with Escape", () => {
    render(
      <SelectControl
        ariaLabel="Stream profile"
        value="main"
        options={options}
        onChange={() => undefined}
      />,
    );

    const trigger = screen.getByRole("combobox", { name: "Stream profile" });
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    expect(screen.getByRole("listbox", { name: "Stream profile options" })).toBeTruthy();
    fireEvent.keyDown(trigger, { key: "Escape" });
    expect(screen.queryByRole("listbox", { name: "Stream profile options" })).toBeNull();
  });
});
