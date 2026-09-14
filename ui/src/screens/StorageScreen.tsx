import { useCallback, useEffect, useState } from "react";
import type { FormEvent } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type { ApplicationSettings, DesktopError, StorageUsage } from "../lib/tauri";

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

function formatBytes(bytes: number | null): string {
  if (bytes === null) return "Unavailable";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let value = bytes / 1024;
  let unit = units[0]!;
  for (let index = 1; index < units.length && value >= 1024; index += 1) {
    value /= 1024;
    unit = units[index]!;
  }
  return `${value >= 100 ? value.toFixed(0) : value >= 10 ? value.toFixed(1) : value.toFixed(2)} ${unit}`;
}

function percentage(value: number, total: number | null): number | null {
  if (total === null || total <= 0) return null;
  return Math.min(100, Math.max(0, (value / total) * 100));
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
  const [usage, setUsage] = useState<StorageUsage | null>(null);
  const [usageLoading, setUsageLoading] = useState(isTauri());
  const [usageError, setUsageError] = useState<DesktopError | null>(null);
  const [saved, setSaved] = useState(false);

  const refreshUsage = useCallback(async () => {
    if (!isTauri()) {
      setUsageLoading(false);
      return;
    }
    setUsageLoading(true);
    setUsageError(null);
    try {
      setUsage(await invokeDesktop<StorageUsage>("storage_usage"));
    } catch (cause) {
      setUsageError(desktopError(cause));
    } finally {
      setUsageLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!isTauri()) {
      setLoading(false);
      return;
    }
    void invokeDesktop<ApplicationSettings>("settings_get")
      .then((settings) => setForm(toForm(settings)))
      .catch((cause) => setError(desktopError(cause)))
      .finally(() => setLoading(false));
    void refreshUsage();
  }, [refreshUsage]);

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
      await refreshUsage();
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setSaving(false);
    }
  }

  const filesystemUsed = usage?.filesystem_total_bytes !== null && usage?.filesystem_total_bytes !== undefined
    && usage.filesystem_available_bytes !== null
    ? Math.max(0, usage.filesystem_total_bytes - usage.filesystem_available_bytes)
    : null;
  const quotaPercent = usage ? percentage(usage.managed_bytes, usage.max_storage_bytes) : null;
  const diskPercent = usage && filesystemUsed !== null ? percentage(filesystemUsed, usage.filesystem_total_bytes) : null;

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

      <section className="panel storage-usage-panel" aria-label="Storage usage">
        <div className="section-heading storage-usage-heading">
          <div><span className="dialog-kicker">Capacity overview</span><h3>Storage usage</h3><p className="muted">Actual managed footage and filesystem capacity, not estimates from configured limits.</p></div>
          <button type="button" onClick={() => void refreshUsage()} disabled={usageLoading}>{usageLoading ? "Refreshing…" : "Refresh usage"}</button>
        </div>
        {usageError ? (
          <p className="warning-message" role="status">{usageError.message}</p>
        ) : usageLoading && !usage ? (
          <p className="muted">Scanning managed footage…</p>
        ) : usage && !usage.configured ? (
          <div className="inline-empty">Choose a footage folder to see storage statistics.</div>
        ) : usage ? (
          <>
            <div className="storage-usage-grid">
              <div className="storage-metric"><span>Managed footage</span><strong>{formatBytes(usage.managed_bytes)}</strong><small>Manual + motion clips</small></div>
              <div className="storage-metric"><span>Free on disk</span><strong>{formatBytes(usage.filesystem_available_bytes)}</strong><small>{usage.filesystem_total_bytes === null ? "Filesystem total unavailable" : `${formatBytes(usage.filesystem_total_bytes)} total`}</small></div>
              <div className="storage-metric"><span>Manual recordings</span><strong>{formatBytes(usage.manual_recording_bytes)}</strong><small>{usage.manual_recording_count} finalized segment{usage.manual_recording_count === 1 ? "" : "s"}</small></div>
              <div className="storage-metric"><span>Motion clips</span><strong>{formatBytes(usage.event_clip_bytes)}</strong><small>{usage.event_clip_count} event clip{usage.event_clip_count === 1 ? "" : "s"}</small></div>
            </div>
            <div className="storage-capacity-bars">
              {diskPercent !== null && filesystemUsed !== null && (
                <div className="storage-capacity-row">
                  <div><span>Disk used</span><strong>{formatBytes(filesystemUsed)} / {formatBytes(usage.filesystem_total_bytes)}</strong></div>
                  <div className="storage-progress" role="progressbar" aria-label="Disk used" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(diskPercent)}><span style={{ width: `${diskPercent}%` }} /></div>
                </div>
              )}
              {quotaPercent !== null && (
                <div className="storage-capacity-row">
                  <div><span>Nian quota</span><strong>{formatBytes(usage.managed_bytes)} / {formatBytes(usage.max_storage_bytes)}</strong></div>
                  <div className="storage-progress" role="progressbar" aria-label="Nian quota usage" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(quotaPercent)}><span style={{ width: `${quotaPercent}%` }} /></div>
                  {usage.cleanup_target_bytes !== null && <small>Cleanup target: {formatBytes(usage.cleanup_target_bytes)}</small>}
                </div>
              )}
            </div>
          </>
        ) : null}
      </section>

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
