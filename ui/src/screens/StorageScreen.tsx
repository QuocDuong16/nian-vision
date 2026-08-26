import { formatBytes } from "../lib/format";
import { EmptyState } from "../components/EmptyState";

export function StorageScreen() {
  // No storage backend wired yet (M4); show the honest current state.
  return (
    <section>
      <EmptyState
        title="Storage not configured"
        hint="Storage location and retention settings arrive in milestone M4."
      />
      <p className="muted">
        Retention target: keep usage below {formatBytes(200 * 1024 * 1024 * 1024)}
      </p>
    </section>
  );
}
