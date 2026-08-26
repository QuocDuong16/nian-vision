import type { ScreenId } from "../navigation";
import { SCREENS } from "../navigation";

export function Sidebar(props: {
  active: ScreenId;
  onNavigate: (screen: ScreenId) => void;
}) {
  return (
    <nav className="sidebar" aria-label="Main navigation">
      <div className="sidebar-brand">NV</div>
      <ul>
        {SCREENS.map((screen) => (
          <li key={screen.id}>
            <button
              type="button"
              className={props.active === screen.id ? "active" : ""}
              onClick={() => props.onNavigate(screen.id)}
            >
              {screen.label}
            </button>
          </li>
        ))}
      </ul>
    </nav>
  );
}
