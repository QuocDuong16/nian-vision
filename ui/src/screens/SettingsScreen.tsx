import { useEffect, useState } from "react";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type { ApplicationSettings, DesktopError } from "../lib/tauri";

export function SettingsScreen() {
  const [settings, setSettings] = useState<ApplicationSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<DesktopError | null>(null);

  useEffect(() => {
    if (!isTauri()) return;
    void invokeDesktop<ApplicationSettings>("settings_get")
      .then(setSettings)
      .catch((cause) => setError(desktopError(cause)));
  }, []);

  async function setLaunchAtLogin(enabled: boolean) {
    if (!settings || saving || !isTauri()) return;
    setSaving(true);
    setError(null);
    try {
      const updated = await invokeDesktop<ApplicationSettings>("settings_update", {
        settings: { ...settings, launch_at_login: enabled },
      });
      setSettings(updated);
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
          <h2>Application settings</h2>
          <p className="muted">Desktop startup behavior is owned by the Rust host and reconciled with the OS registration.</p>
        </div>
      </div>
      {error && <div className="error-banner" role="alert"><strong>{error.code}</strong>: {error.message}</div>}
      <div className="panel form-grid">
        <h3>Desktop lifecycle</h3>
        <label className="checkbox-row">
          <input
            aria-label="Launch at login"
            type="checkbox"
            checked={settings?.launch_at_login ?? false}
            disabled={!settings || saving}
            onChange={(event) => void setLaunchAtLogin(event.target.checked)}
          />
          <span>Launch at login</span>
        </label>
        <p className="muted">Login launch starts hidden in the system tray. Manual launch opens the main window.</p>
      </div>
      <div className="panel">
        <h3>Window behavior</h3>
        <p>Closing the main window hides Nian Vision to the tray. Recording continues. Use tray Quit for a graceful process shutdown.</p>
      </div>
    </section>
  );
}
