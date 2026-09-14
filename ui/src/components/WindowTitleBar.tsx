import { getCurrentWindow } from "@tauri-apps/api/window";
import { isTauri } from "../lib/tauri";

function runWindowAction(action: "minimize" | "toggleMaximize" | "close") {
  if (!isTauri()) return;
  const appWindow = getCurrentWindow();
  if (action === "minimize") void appWindow.minimize();
  else if (action === "toggleMaximize") void appWindow.toggleMaximize();
  else void appWindow.close();
}

export function WindowTitleBar({ title = "Nian Vision" }: { title?: string }) {
  return (
    <div className="window-titlebar" data-tauri-drag-region>
      <div className="window-titlebar-brand" data-tauri-drag-region>
        <span className="window-titlebar-mark" aria-hidden="true">NV</span>
        <span className="window-titlebar-title" data-tauri-drag-region>{title}</span>
      </div>
      <div className="window-titlebar-controls" role="group" aria-label="Window controls">
        <button
          type="button"
          className="window-control"
          aria-label="Minimize window"
          title="Minimize"
          onClick={() => runWindowAction("minimize")}
        >
          <span aria-hidden="true" className="window-control-minimize" />
        </button>
        <button
          type="button"
          className="window-control"
          aria-label="Maximize or restore window"
          title="Maximize / Restore"
          onClick={() => runWindowAction("toggleMaximize")}
        >
          <span aria-hidden="true" className="window-control-maximize" />
        </button>
        <button
          type="button"
          className="window-control window-control-close"
          aria-label="Close window"
          title="Close"
          onClick={() => runWindowAction("close")}
        >
          <span aria-hidden="true" className="window-control-close-glyph">×</span>
        </button>
      </div>
    </div>
  );
}
