import type { CameraState } from "../types";

const STATE_CLASS: Record<CameraState, string> = {
  idle: "chip chip-idle",
  connecting: "chip chip-connecting",
  recording: "chip chip-recording",
  reconnecting: "chip chip-reconnecting",
  offline: "chip chip-offline",
  error: "chip chip-error",
};

export function StatusChip(props: { state: CameraState }) {
  return <span className={STATE_CLASS[props.state]}>{props.state}</span>;
}
