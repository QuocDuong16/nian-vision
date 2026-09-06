import { useEffect, useState } from "react";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type { AppInfo, ApplicationSettings, DesktopError, NotificationSettings, UpdateCheck } from "../lib/tauri";

export function SettingsScreen() {
  const [settings, setSettings] = useState<ApplicationSettings | null>(null);
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [update, setUpdate] = useState<UpdateCheck | null>(null);
  const [notificationSettings, setNotificationSettings] = useState<NotificationSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [notificationSaving, setNotificationSaving] = useState(false);
  const [checking, setChecking] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState<DesktopError | null>(null);

  useEffect(() => {
    if (!isTauri()) return;
    void Promise.all([
      invokeDesktop<ApplicationSettings>("settings_get").then(setSettings),
      invokeDesktop<AppInfo>("app_info").then(setAppInfo),
      invokeDesktop<NotificationSettings>("notification_settings_get").then(setNotificationSettings),
    ]).catch((cause) => setError(desktopError(cause)));
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

  async function setMotionNotifications(enabled: boolean) {
    if (!notificationSettings || notificationSaving || !isTauri()) return;
    setNotificationSaving(true);
    setError(null);
    try {
      const updated = await invokeDesktop<NotificationSettings>("notification_settings_update", {
        motionNotificationsEnabled: enabled,
      });
      setNotificationSettings(updated);
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setNotificationSaving(false);
    }
  }

  async function checkForUpdates() {
    if (checking || installing || !isTauri()) return;
    setChecking(true);
    setError(null);
    try {
      setUpdate(await invokeDesktop<UpdateCheck>("update_check"));
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setChecking(false);
    }
  }

  async function installUpdate() {
    if (!update?.available || installing || !isTauri()) return;
    const approved = window.confirm(
      `Install Nian Vision ${update.available.version}? Active recording will stop cleanly and resume after the application restarts.`,
    );
    if (!approved) return;

    setInstalling(true);
    setError(null);
    try {
      await invokeDesktop<void>("update_install", { expectedVersion: update.available.version });
    } catch (cause) {
      setError(desktopError(cause));
      setInstalling(false);
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
      <div className="panel form-grid">
        <h3>Notifications</h3>
        <label className="checkbox-row">
          <input
            aria-label="Desktop motion notifications"
            type="checkbox"
            checked={notificationSettings?.motion_notifications_enabled ?? false}
            disabled={!notificationSettings || !notificationSettings.supported || notificationSaving}
            onChange={(event) => void setMotionNotifications(event.target.checked)}
          />
          <span>Desktop motion notifications</span>
        </label>
        {notificationSettings && !notificationSettings.supported ? (
          <p className="muted">Unavailable on this system.</p>
        ) : (
          <p className="muted">Off by default. Only newly persisted MotionStarted events are eligible.</p>
        )}
      </div>
      <div className="panel">
        <h3>Updates</h3>
        <p>Current version: <strong>{appInfo?.version ?? "unknown"}</strong></p>
        <div className="button-row">
          <button type="button" disabled={checking || installing || !isTauri()} onClick={() => void checkForUpdates()}>
            {checking ? "Checking…" : "Check for updates"}
          </button>
          {update?.available && (
            <button type="button" disabled={installing} onClick={() => void installUpdate()}>
              {installing ? "Installing…" : `Download and install ${update.available.version}`}
            </button>
          )}
        </div>
        {update && !update.configured && (
          <p className="muted">This build has no production update channel configured.</p>
        )}
        {update?.configured && !update.available && (
          <p className="muted">Nian Vision is up to date.</p>
        )}
        {update?.available && (
          <div className="update-summary">
            <p><strong>Version {update.available.version} is available.</strong></p>
            {update.available.notes && <p>{update.available.notes}</p>}
            {update.available.date && <p className="muted">Published {update.available.date}</p>}
            <p className="muted">Installing requires an explicit restart. Recording intent is preserved and restored after startup.</p>
          </div>
        )}
      </div>
      <div className="panel">
        <h3>Window behavior</h3>
        <p>Closing the main window hides Nian Vision to the tray. Recording continues. Use tray Quit for a graceful process shutdown.</p>
      </div>
    </section>
  );
}
