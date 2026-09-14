import { useState } from "react";
import type { ChangeEvent } from "react";

type PasswordInputProps = {
  ariaLabel: string;
  value: string;
  onChange: (event: ChangeEvent<HTMLInputElement>) => void;
  placeholder?: string;
  disabled?: boolean;
  maxLength?: number;
  autoComplete?: string;
};

export function PasswordInput({
  ariaLabel,
  value,
  onChange,
  placeholder,
  disabled = false,
  maxLength,
  autoComplete = "new-password",
}: PasswordInputProps) {
  const [visible, setVisible] = useState(false);
  return (
    <div className="password-input">
      <input
        aria-label={ariaLabel}
        type={visible ? "text" : "password"}
        autoComplete={autoComplete}
        value={value}
        onChange={onChange}
        placeholder={placeholder}
        disabled={disabled}
        maxLength={maxLength}
      />
      <button
        type="button"
        className="password-visibility-button"
        aria-label={`${visible ? "Hide" : "Show"} ${ariaLabel.toLowerCase()}`}
        aria-pressed={visible}
        disabled={disabled}
        onClick={() => setVisible((current) => !current)}
      >
        {visible ? "Hide" : "Show"}
      </button>
    </div>
  );
}
