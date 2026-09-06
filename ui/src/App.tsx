import { useState } from "react";
import type { AppInfo } from "./lib/tauri";
import { useTauriCommand } from "./lib/tauri";
import type { ScreenId } from "./navigation";
import { Sidebar } from "./components/Sidebar";
import { CamerasScreen } from "./screens/CamerasScreen";
import { LiveViewScreen } from "./screens/LiveViewScreen";
import { EventReviewScreen } from "./screens/EventReviewScreen";
import { TimelineScreen } from "./screens/TimelineScreen";
import { StorageScreen } from "./screens/StorageScreen";
import { SettingsScreen } from "./screens/SettingsScreen";

const FALLBACK_APP_INFO: AppInfo = {
  name: "Nian Vision",
  version: "browser preview",
};

export function App() {
  const [screen, setScreen] = useState<ScreenId>("cameras");
  const appInfo = useTauriCommand<AppInfo>("app_info", FALLBACK_APP_INFO);

  return (
    <div className="app-shell">
      <Sidebar active={screen} onNavigate={setScreen} />
      <main className="app-content">
        <header className="app-header">
          <h1>{appInfo.name}</h1>
          <span className="app-version">v{appInfo.version}</span>
        </header>
        {screen === "cameras" && <CamerasScreen />}
        {screen === "live" && <LiveViewScreen />}
        {screen === "events" && <EventReviewScreen />}
        {screen === "timeline" && <TimelineScreen />}
        {screen === "storage" && <StorageScreen />}
        {screen === "settings" && <SettingsScreen />}
      </main>
    </div>
  );
}
