import type { ScreenId } from "../navigation";
import { SCREENS } from "../navigation";

function NavigationIcon({ id }: { id: ScreenId }) {
  const common = {
    width: 20,
    height: 20,
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.8,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    "aria-hidden": true,
  };

  if (id === "cameras") return <svg {...common}><path d="M4 7.5h11a2 2 0 0 1 2 2v5a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-5a2 2 0 0 1 2-2Z" /><path d="m17 10 5-2.5v9L17 14" /><circle cx="8.5" cy="12" r="2" /></svg>;
  if (id === "live") return <svg {...common}><rect x="3" y="4" width="18" height="14" rx="2" /><path d="m10 9 5 3-5 3Z" /><path d="M8 21h8" /></svg>;
  if (id === "events") return <svg {...common}><path d="M12 3a6 6 0 0 0-6 6v3l-2 3h16l-2-3V9a6 6 0 0 0-6-6Z" /><path d="M10 19h4" /><path d="M12 7v3" /></svg>;
  if (id === "timeline") return <svg {...common}><circle cx="12" cy="12" r="9" /><path d="M12 7v5l3 2" /><path d="M3 12h2M19 12h2" /></svg>;
  if (id === "storage") return <svg {...common}><ellipse cx="12" cy="5" rx="8" ry="3" /><path d="M4 5v7c0 1.7 3.6 3 8 3s8-1.3 8-3V5" /><path d="M4 12v7c0 1.7 3.6 3 8 3s8-1.3 8-3v-7" /></svg>;
  return <svg {...common}><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.7 1.7 0 0 0 .34 1.88l.06.06-2.83 2.83-.06-.06A1.7 1.7 0 0 0 15 19.4a1.7 1.7 0 0 0-1 .6 1.7 1.7 0 0 0-.4 1.1V21h-4v-.09A1.7 1.7 0 0 0 8.6 19.4a1.7 1.7 0 0 0-1.88.34l-.06.06-2.83-2.83.06-.06A1.7 1.7 0 0 0 4.6 15a1.7 1.7 0 0 0-.6-1 1.7 1.7 0 0 0-1.1-.4H3v-4h.09A1.7 1.7 0 0 0 4.6 8.6a1.7 1.7 0 0 0-.34-1.88l-.06-.06 2.83-2.83.06.06A1.7 1.7 0 0 0 9 4.6a1.7 1.7 0 0 0 1-.6 1.7 1.7 0 0 0 .4-1.1V3h4v.09A1.7 1.7 0 0 0 15.4 4.6a1.7 1.7 0 0 0 1.88-.34l.06-.06 2.83 2.83-.06.06A1.7 1.7 0 0 0 19.4 9c.12.37.34.7.64.95.3.25.67.39 1.06.4H21v4h-.09A1.7 1.7 0 0 0 19.4 15Z" /></svg>;
}

export function Sidebar(props: {
  active: ScreenId;
  onNavigate: (screen: ScreenId) => void;
}) {
  return (
    <nav className="sidebar" aria-label="Main navigation">
      <div className="sidebar-brand-block">
        <div className="sidebar-brand" aria-hidden="true">NV</div>
        <div className="sidebar-brand-copy">
          <strong>Nian Vision</strong>
          <span>Video management</span>
        </div>
      </div>
      <div className="sidebar-section-label">Workspace</div>
      <ul className="sidebar-nav-list">
        {SCREENS.map((screen) => (
          <li key={screen.id}>
            <button
              type="button"
              className={props.active === screen.id ? "active" : ""}
              onClick={() => props.onNavigate(screen.id)}
              aria-label={screen.label}
              title={`${screen.label} · ${screen.description}`}
            >
              <span className="sidebar-nav-icon"><NavigationIcon id={screen.id} /></span>
              <span className="sidebar-nav-copy">
                <span className="sidebar-nav-label">{screen.label}</span>
                <span className="sidebar-nav-description">{screen.description}</span>
              </span>
            </button>
          </li>
        ))}
      </ul>
      <div className="sidebar-footer">
        <span className="sidebar-health-dot" aria-hidden="true" />
        <span>Local desktop</span>
      </div>
    </nav>
  );
}
