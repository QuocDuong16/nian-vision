import { useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { CSSProperties, KeyboardEvent } from "react";

export type SelectOption = {
  value: string;
  label: string;
  disabled?: boolean;
  description?: string;
};

type SelectControlProps = {
  ariaLabel: string;
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  disabled?: boolean;
  placeholder?: string;
  className?: string;
};

export function SelectControl({
  ariaLabel,
  value,
  options,
  onChange,
  disabled = false,
  placeholder = "Select…",
  className = "",
}: SelectControlProps) {
  const [open, setOpen] = useState(false);
  const [menuStyle, setMenuStyle] = useState<CSSProperties>({});
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const listboxId = useId();
  const selected = options.find((option) => option.value === value);

  useEffect(() => {
    if (!open) return;

    const placeMenu = () => {
      const trigger = triggerRef.current;
      if (!trigger) return;
      const rect = trigger.getBoundingClientRect();
      const viewportPadding = 10;
      const gap = 6;
      const availableBelow = window.innerHeight - rect.bottom - viewportPadding - gap;
      const availableAbove = rect.top - viewportPadding - gap;
      const placeAbove = availableBelow < 180 && availableAbove > availableBelow;
      const availableHeight = Math.max(96, placeAbove ? availableAbove : availableBelow);
      const maxHeight = Math.min(280, availableHeight);
      const viewportWidth = Math.max(0, window.innerWidth - viewportPadding * 2);
      const width = Math.min(Math.max(rect.width, 180), viewportWidth);
      const maxLeft = Math.max(viewportPadding, window.innerWidth - viewportPadding - width);
      const left = Math.min(Math.max(viewportPadding, rect.left), maxLeft);

      setMenuStyle({
        position: "fixed",
        left,
        width,
        maxHeight,
        ...(placeAbove
          ? { bottom: window.innerHeight - rect.top + gap, top: "auto" }
          : { top: rect.bottom + gap, bottom: "auto" }),
      });
    };

    const close = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!rootRef.current?.contains(target) && !menuRef.current?.contains(target)) setOpen(false);
    };

    placeMenu();
    window.addEventListener("pointerdown", close, true);
    window.addEventListener("resize", placeMenu);
    window.addEventListener("scroll", placeMenu, true);
    return () => {
      window.removeEventListener("pointerdown", close, true);
      window.removeEventListener("resize", placeMenu);
      window.removeEventListener("scroll", placeMenu, true);
    };
  }, [open]);

  useEffect(() => {
    if (disabled) setOpen(false);
  }, [disabled]);

  const choose = (next: string) => {
    const option = options.find((candidate) => candidate.value === next);
    if (!option || option.disabled) return;
    onChange(next);
    setOpen(false);
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (disabled) return;
    if (event.key === "Escape") {
      setOpen(false);
      return;
    }
    if (event.key === "Enter" || event.key === " " || event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      setOpen(true);
    }
  };

  return (
    <div ref={rootRef} className={`select-control ${open ? "is-open" : ""} ${className}`.trim()}>
      <select
        className="select-control-native-proxy"
        aria-label={`${ariaLabel} native value`}
        aria-hidden="true"
        tabIndex={-1}
        value={value}
        disabled={disabled}
        onChange={(event) => choose(event.target.value)}
      >
        {options.map((option) => (
          <option key={option.value} value={option.value} disabled={option.disabled}>{option.label}</option>
        ))}
      </select>
      <button
        ref={triggerRef}
        type="button"
        className="select-control-trigger"
        role="combobox"
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={listboxId}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
        onKeyDown={handleKeyDown}
      >
        <span className={`select-control-value ${selected ? "" : "is-placeholder"}`.trim()}>{selected?.label ?? placeholder}</span>
        <span className="select-control-caret" aria-hidden="true" />
      </button>
      {open && typeof document !== "undefined" && createPortal(
        <div ref={menuRef} className="select-control-menu" id={listboxId} role="listbox" aria-label={`${ariaLabel} options`} style={menuStyle}>
          {options.map((option) => (
            <button
              key={option.value}
              type="button"
              role="option"
              aria-selected={option.value === value}
              className={`select-control-option ${option.value === value ? "is-selected" : ""}`}
              disabled={option.disabled}
              onClick={() => choose(option.value)}
            >
              <span>{option.label}</span>
              {option.description && <small>{option.description}</small>}
            </button>
          ))}
        </div>,
        document.body,
      )}
    </div>
  );
}
