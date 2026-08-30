import { useEffect, useState } from "react";
import type { FormEvent } from "react";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type { ApplicationSettings, DesktopError } from "../lib/tauri";

interface SettingsForm {
  storage_root: string;
  segment_target_secs: string;
  max_age_days: string;
  max_storage_bytes: string;
  cleanup_target_bytes: string;
  launch_at_login: boolean;
}

const EMPTY: SettingsForm = {
  storage_root: "",
  segment_target_secs: "300",
  max_age_days: "",
  max_storage_bytes: "",
  cleanup_target_bytes: "",
  launch_at_login: false,
};

function toForm(settings: ApplicationSettings): SettingsForm {
  return {
    storage_root: settings.storage_root ?? "",
    segment_target_secs: String(settings.segment_target_secs),
    max_age_days: settings.max_age_days === null ? "" : String(settings.max_age_days),
    max_storage_bytes: settings.max_storage_bytes === null ? "" : String(settings.max_storage_bytes),
    cleanup_target_bytes: settings.cleanup_target_bytes === null ? "" : String(settings.cleanup_target_bytes),
    launch_at_login: settings.launch_at_login,
  };
}

function positiveInteger(value: string, label: string): number | null {
  if (!value.trim()) return null;
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) throw new Error(`${label} must be a positive integer.`);
  return parsed;
}

export function StorageScreen() {
  const [form, setForm] = useState<SettingsForm>(EMPTY);
  const [loading, setLoading] = useState(isTauri());
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<DesktopError | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (!isTauri()) {
      setLoading(false);
      return;
    }
    void invokeDesktop<ApplicationSettings>("settings_get")
      .then((settings) => setForm(toForm(settings)))
      .catch((cause) => setError(desktopError(cause)))
      .finally(() => setLoading(false));
  }, []);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (saving) return;
    setSaved(false);
    setError(null);
    let settings: ApplicationSettings;
    try {
      if (!form.storage_root.trim()) throw new Error("Recordings storage root is required.");
      const segment = positiveInteger(form.segment_target_secs, "Segment target");
      if (segment === null || segment < 5 || segment > 3600) throw new Error("Segment target must be between 5 and 3600 seconds.");
      const maxStorageBytes = positiveInteger(form.max_storage_bytes, "Max storage bytes");
      const cleanupTargetBytes = positiveInteger(form.cleanup_target_bytes, "Cleanup target bytes");
      if ((maxStorageBytes === null) !== (cleanupTargetBytes === null)) {
        throw new Error("Max storage bytes and cleanup target bytes must be configured together.");
      }
      if (maxStorageBytes !== null && maxStorageBytes < 1_048_576) {
        throw new Error("Max storage bytes must be at least 1048576 bytes.");
      }
      if (maxStorageBytes !== null && cleanupTargetBytes !== null && cleanupTargetBytes >= maxStorageBytes) {
        throw new Error("Cleanup target bytes must be lower than max storage bytes.");
      }
      settings = {
        storage_root: form.storage_root.trim(),
        segment_target_secs: segment,
        max_age_days: positiveInteger(form.max_age_days, "Max age days"),
        max_storage_bytes: maxStorageBytes,
        cleanup_target_bytes: cleanupTargetBytes,
        launch_at_login: form.launch_at_login,
      };
    } catch (cause) {
      setError({ code: "validation", message: cause instanceof Error ? cause.message : "Invalid settings." });
      return;
    }
    if (!isTauri()) {
      setError({ code: "internal", message: "Storage settings require the desktop host." });
      return;
    }
    setSaving(true);
    try {
      const result = await invokeDesktop<ApplicationSettings>("settings_update", { settings });
      setForm(toForm(result));
      setSaved(true);
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="screen-stack">
      <div className="screen-toolbar">
        <div>
          <h2>Storage</h2>
          <p className="muted">Authoritative recorder settings. Changing these while recording is rejected.</p>
        </div>
      </div>
      {error && <div className="error-banner" role="alert"><strong>{error.code}</strong>: {error.message}</div>}
      {saved && <p className="success-message" role="status">Storage settings saved.</p>}
      {loading ? <p className="muted" role="status">Loading storage settings…</p> : (
        <form className="panel form-grid storage-form" onSubmit={(event) => void submit(event)}>
          <label className="full-width">Recordings storage root
            <input
              aria-label="Recordings storage root"
              value={form.storage_root}
              onChange={(event) => setForm({ ...form, storage_root: event.target.value })}
              placeholder="D:\\Nian Vision Recordings"
            />
          </label>
          <label>Segment target (seconds)
            <input aria-label="Segment target" type="number" min={5} max={3600} value={form.segment_target_secs} onChange={(event) => setForm({ ...form, segment_target_secs: event.target.value })} />
          </label>
          <label>Max age (days, optional)
            <input aria-label="Max age days" type="number" min={1} value={form.max_age_days} onChange={(event) => setForm({ ...form, max_age_days: event.target.value })} />
          </label>
          <label>Max storage bytes (optional HIGH watermark)
            <input aria-label="Max storage bytes" type="number" min={1048576} value={form.max_storage_bytes} onChange={(event) => setForm({ ...form, max_storage_bytes: event.target.value })} />
          </label>
          <label>Cleanup target bytes (optional LOW watermark)
            <input aria-label="Cleanup target bytes" type="number" min={1} value={form.cleanup_target_bytes} onChange={(event) => setForm({ ...form, cleanup_target_bytes: event.target.value })} />
          </label>
          <div className="form-actions full-width">
            <button className="primary-button" type="submit" disabled={saving}>{saving ? "Saving…" : "Save storage settings"}</button>
          </div>
        </form>
      )}
    </section>
  );
}
