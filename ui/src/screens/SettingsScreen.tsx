export function SettingsScreen() {
  return (
    <section className="screen-stack">
      <div className="screen-toolbar">
        <div>
          <h2>Application settings</h2>
          <p className="muted">Camera and storage configuration is available in M5. Tray, autostart and power behavior remain M7 scope.</p>
        </div>
      </div>
      <div className="panel">
        <h3>Recording restore policy</h3>
        <p>Recording state is session-only in M5. Saved cameras and storage settings persist after restart, but recording does not auto-start.</p>
      </div>
    </section>
  );
}
