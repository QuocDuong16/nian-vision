export type ScreenId = "cameras" | "live" | "timeline" | "storage" | "settings";

export interface ScreenDefinition {
  id: ScreenId;
  label: string;
}

export const SCREENS: ScreenDefinition[] = [
  { id: "cameras", label: "Cameras" },
  { id: "live", label: "Live View" },
  { id: "timeline", label: "Timeline" },
  { id: "storage", label: "Storage" },
  { id: "settings", label: "Settings" },
];
