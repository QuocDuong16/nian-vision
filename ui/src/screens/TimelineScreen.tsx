import { EmptyState } from "../components/EmptyState";

export function TimelineScreen() {
  return (
    <EmptyState
      title="No recordings yet"
      hint="The recording timeline appears once cameras record footage (milestone M6)."
    />
  );
}
