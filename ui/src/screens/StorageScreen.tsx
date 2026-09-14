import { useEffect, useState } from "react";
import type { FormEvent } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type { ApplicationSettings, DesktopError } from "../lib/tauri";

const GIB = 1024 ** 3;
const MIN_STORAGE_BYTES = 1024 ** 2;

interface SettingsForm {
  storage_root: string;
  segment_target_secs: string;
  max_age_days: string;
  max_storage_gib: string;
  cleanup_target_gib: string;
  launch_at_login: boolean;
}

const EMPTY: SettingsForm = {
  storage_root: "",
  segment_target_secs: "300",
  max_age_days: "",
  max_storage_gib: "",
  cleanup_target_gib: "",
  launch_at_login: false,
};

function formatGiB(bytes: number | null): string {
  if (bytes === null) return "";
  const value = bytes / GIB;
  return Number.isInteger(value) ? String(value) : String(Number(value.toFixed(3)));
}

function toForm(settings: ApplicationSettings): SettingsForm {
  return {
    storage_root: settings.storage_root ?? "",
    segment_target_secs: String(settings.segment_target_secs),
    max_age_days: settings.max_age_days === null ? "" : String(settings.max_age_days),
    max_storage_gib: formatGiB(settings.max_storage_bytes),
    cleanup_target_gib: formatGiB(settings.cleanup_target_bytes),
    launch_at_login: settings.launch_at_login,
  };
}

function positiveInteger(value: string, label: string): number | null {
  if (!value.trim()) return null;
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) throw new Error(`${label} must be a positive integer.`);
  return parsed;
}

function gibToBytes(value: string, label: string): number | null {
  if (!value.trim()) return null;
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed <= 0) throw new Error(`${label} must be greater than zero.`);
  const bytes = Math.round(parsed * GIB);
  if (!Number.isSafeInteger(bytes)) throw new Error(`${label} is too large.`);
  if (bytes < MIN_STORAGE_BYTES) throw new Error(`${label} must be at least 0.001 GB.`);
  return bytes;
}

export function StorageScreen() {
  const [form, setForm] = useState<SettingsForm>(EMPTY);
  const [loading, setLoading] = useState(isTauri());
  const [saving, setSaving] = useState(false);
  const [pickingFolder, setPickingFolder] = useState(false);
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

  async function chooseStorageRoot() {
    if (!isTauri() || pickingFolder) return;
    setPickingFolder(true);
    setError(null);
    try {
      const currentRoot = form.storage_root.trim();
      const selected = await openDialog({
        directory: true,
        multiple: false,
        title: "Choose recordings folder",
        ...(currentRoot ? { defaultPath: currentRoot } : {}),
      });
      if (typeof selected === "string") {
        setForm((current) => ({ ...current, storage_root: selected }));
        setSaved(false);
      }
    } catch (cause) {
      setError({ code: "folder_picker", message: cause instanceof Error ? cause.message : "Could not open the folder picker." });
    } finally {
      setPickingFolder(false);
    }
  }

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
      const maxStorageBytes = gibToBytes(form.max_storage_gib, "Storage high watermark");
      const cleanupTargetBytes = gibToBytes(form.cleanup_target_gib, "Cleanup low watermark");
      if ((maxStorageBytes === null) !== (cleanupTargetBytes === null)) {
        throw new Error("Storage high and cleanup low watermarks must be configured together.");
      }
      if (maxStorageBytes !== null && cleanupTargetBytes !== null && cleanupTargetBytes >= maxStorageBytes) {
        throw new Error("Cleanup low watermark must be lower than the storage high watermark.");
      }
      settings = {
        storage_root: form.storage_root.trim(),
        segment_target_secs: segment,
        max_age_days: positiveInteger(form.max_age_days, "Retention age"),
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
    <section className="screen-stack storage-screen">
      <div className="screen-toolbar">
        <div>
          <h2>Storage</h2>
          <p className="muted">Choose where footage lives and when Nian Vision should reclaim older recordings.</p>
        </div>
      </div>
      {error && <div className="error-banner" role="alert"><strong>{error.code}</strong>: {error.message}</div>}
      {saved && <p className="success-message" role="status">Storage settings saved.</p>}
      {loading ? <p className="muted" role="status">Loading storage settings…</p> : (
        <form className="storage-workspace" onSubmit={(event) => void submit(event)}>
          <section className="panel storage-section storage-root-section">
            <div className="section-heading">
              <div><span className="dialog-kicker">Recording location</span><h3>Footage folder</h3><p className="muted">Manual recordings and Nian-managed event media are stored below this folder.</p></div>
              <span className="storage-status-badge">Local storage</span>
            </div>
            <label className="full-width">Recordings storage root
              <div className="path-picker-control">
                <input
                  aria-label="Recordings storage root"
                  value={form.storage_root}
                  onChange={(event) => { setForm({ ...form, storage_root: event.target.value }); setSaved(false); }}
                  placeholder="D:\\Nian Vision Recordings"
                />
                <button className="secondary-action-button browse-folder-button" type="button" onClick={() => void chooseStorageRoot()} disabled={pickingFolder || saving}>
                  {pickingFolder ? "Opening…" : "Browse folder"}
                </button>
              </div>
            </label>
          </section>

          <div className="storage-settings-grid">
            <section className="panel storage-section">
              <div className="section-heading"><div><span className="dialog-kicker">Recording files</span><h3>Segment duration</h3><p className="muted">How often an active manual recording is finalized into a durable segment.</p></div></div>
              <label>Target duration
                <div className="field-with-unit"><input aria-label="Segment target" type="number" min={5} max={3600} value={form.segment_target_secs} onChange={(event) => setForm({ ...form, segment_target_secs: event.target.value })} /><span>sec</span></div>
                <span className="field-help">5–3600 seconds. The default is 300 seconds.</span>
              </label>
            </section>

            <section className="panel storage-section">
              <div className="section-heading"><div><span className="dialog-kicker">Retention</span><h3>Age limit</h3><p className="muted">Optionally remove eligible footage after it becomes older than this limit.</p></div></div>
              <label>Maximum age
                <div className="field-with-unit"><input aria-label="Max age days" type="number" min={1} value={form.max_age_days} onChange={(event) => setForm({ ...form, max_age_days: event.target.value })} placeholder="No age limit" /><span>days</span></div>
              </label>
            </section>

            <section className="panel storage-section storage-quota-section">
              <div className="section-heading"><div><span className="dialog-kicker">Capacity guardrail</span><h3>Storage cleanup</h3><p className="muted">Cleanup starts at the high watermark and stops after usage falls below the low watermark.</p></div><span className="optional-badge">Optional</span></div>
              <div className="storage-watermark-grid">
                <label>High watermark
                  <div className="field-with-unit"><input aria-label="Storage high watermark GB" type="number" min="0.001" step="0.001" value={form.max_storage_gib} onChange={(event) => setForm({ ...form, max_storage_gib: event.target.value })} placeholder="e.g. 500" /><span>GB</span></div>
                  <span className="field-help">Cleanup begins after recording usage crosses this amount.</span>
                </label>
                <label>Low watermark
                  <div className="field-with-unit"><input aria-label="Cleanup low watermark GB" type="number" min="0.001" step="0.001" value={form.cleanup_target_gib} onChange={(event) => setForm({ ...form, cleanup_target_gib: event.target.value })} placeholder="e.g. 450" /><span>GB</span></div>
                  <span className="field-help">Must be lower than the high watermark.</span>
                </label>
              </div>
            </section>
          </div>

          <div className="storage-save-bar">
            <span className="muted">Changes are rejected while recording is active.</span>
            <button className="primary-button" type="submit" disabled={saving || pickingFolder}>{saving ? "Saving…" : "Save storage settings"}</button>
          </div>
        </form>
      )}
    </section>
  );
}
