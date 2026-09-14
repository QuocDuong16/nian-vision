import { useEffect, useState } from "react";
import type { AppInfo } from "./lib/tauri";
import { isTauri, useTauriCommand } from "./lib/tauri";
import { SCREENS, type ScreenId } from "./navigation";
import { Sidebar } from "./components/Sidebar";
import { WindowTitleBar } from "./components/WindowTitleBar";
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
  const appInfo = useTauriCommand<AppInfo>("app_info", FALLBACK_APP_INFO);
  const activeScreenLabel = SCREENS.find((entry) => entry.id === screen)?.label ?? "Cameras";

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
    setScreen(next);
  };

  return (
    <div className="desktop-frame">
      <WindowTitleBar title={appInfo.name} />
      <div className="app-shell">
        <Sidebar active={screen} onNavigate={navigate} />
        <main className="app-content">
          <header className="app-header">
            <div className="app-header-context">
              <span className="app-header-eyebrow">{appInfo.name}</span>
              <span className="app-header-separator" aria-hidden="true">/</span>
              <span className="app-header-section">{activeScreenLabel}</span>
            </div>
            <div className="app-header-meta">
              <span className="app-runtime-badge"><span className="app-runtime-dot" aria-hidden="true" /> Desktop</span>
              <span className="app-version">v{appInfo.version}</span>
            </div>
          </header>
          {screen === "cameras" && <CamerasScreen />}
          {screen === "live" && <LiveViewScreen />}
          {screen === "events" && <EventReviewScreen />}
          {screen === "timeline" && <TimelineScreen />}
          {screen === "storage" && <StorageScreen />}
          {screen === "settings" && <SettingsScreen />}
        </main>
      </div>
    </div>
  );
}
