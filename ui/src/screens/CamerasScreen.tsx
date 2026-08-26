import { EmptyState } from "../components/EmptyState";

export function CamerasScreen() {
  return (
    <EmptyState
      title="No cameras configured"
      hint="Add a camera to start continuous recording. Camera management arrives in milestone M5."
    />
  );
}
