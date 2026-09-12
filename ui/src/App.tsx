import { useEffect, useState } from "react";
import type { AppInfo } from "./lib/tauri";
import { isTauri, useTauriCommand } from "./lib/tauri";
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

export function isDesktopReloadShortcut(event: Pick<KeyboardEvent, "key" | "ctrlKey" | "metaKey">): boolean {
  const key = event.key.toLowerCase();
  return key === "f5" || ((event.ctrlKey || event.metaKey) && key === "r");
}

export function App() {
  const [screen, setScreen] = useState<ScreenId>("cameras");
  const [liveVisited, setLiveVisited] = useState(false);
  const appInfo = useTauriCommand<AppInfo>("app_info", FALLBACK_APP_INFO);

  useEffect(() => {
    if (!isTauri()) return;
    const blockContextMenu = (event: MouseEvent) => event.preventDefault();
    const blockReloadShortcut = (event: KeyboardEvent) => {
      if (isDesktopReloadShortcut(event)) {
        event.preventDefault();
        event.stopPropagation();
      }
    };
    window.addEventListener("contextmenu", blockContextMenu);
    window.addEventListener("keydown", blockReloadShortcut, true);
    return () => {
      window.removeEventListener("contextmenu", blockContextMenu);
      window.removeEventListener("keydown", blockReloadShortcut, true);
    };
  }, []);

  const navigate = (next: ScreenId) => {
    if (next === "live") setLiveVisited(true);
    setScreen(next);
  };

  return (
    <div className="app-shell">
      <Sidebar active={screen} onNavigate={navigate} />
      <main className="app-content">
        <header className="app-header">
          <h1>{appInfo.name}</h1>
          <span className="app-version">v{appInfo.version}</span>
        </header>
        {screen === "cameras" && <CamerasScreen />}
        {liveVisited && (
          <section data-resident-screen="live" hidden={screen !== "live"}>
            <LiveViewScreen />
          </section>
        )}
        {screen === "events" && <EventReviewScreen />}
        {screen === "timeline" && <TimelineScreen />}
        {screen === "storage" && <StorageScreen />}
        {screen === "settings" && <SettingsScreen />}
      </main>
    </div>
  );
}
