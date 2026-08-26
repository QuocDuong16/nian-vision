/** Camera lifecycle states, mirroring `nian_domain::CameraState`. */
export type CameraState =
  | "idle"
  | "connecting"
  | "recording"
  | "reconnecting"
  | "offline"
  | "error";
