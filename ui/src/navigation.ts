export type ScreenId = "cameras" | "live" | "events" | "timeline" | "storage" | "settings";

export interface ScreenDefinition {
  id: ScreenId;
  label: string;
}

export const SCREENS: ScreenDefinition[] = [
  { id: "cameras", label: "Cameras" },
  { id: "live", label: "Live View" },
  { id: "events", label: "Events" },
  { id: "timeline", label: "Timeline" },
  { id: "storage", label: "Storage" },
  { id: "settings", label: "Settings" },
];
