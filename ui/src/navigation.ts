export type ScreenId = "cameras" | "live" | "events" | "timeline" | "storage" | "settings";

export interface ScreenDefinition {
  id: ScreenId;
  label: string;
  description: string;
}

export const SCREENS: ScreenDefinition[] = [
  { id: "cameras", label: "Cameras", description: "Devices & recording" },
  { id: "live", label: "Live View", description: "Realtime monitoring" },
  { id: "events", label: "Events", description: "Motion review" },
  { id: "timeline", label: "Timeline", description: "Manual recordings" },
  { id: "storage", label: "Storage", description: "Retention & capacity" },
  { id: "settings", label: "Settings", description: "Application" },
];
