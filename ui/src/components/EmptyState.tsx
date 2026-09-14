export function EmptyState(props: { title: string; hint?: string }) {
  return (
    <div className="empty-state">
      <div className="empty-state-icon" aria-hidden="true">
        <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
          <rect x="3" y="5" width="18" height="14" rx="3" />
          <circle cx="12" cy="12" r="3" />
          <path d="M8 5 9.5 3h5L16 5" />
        </svg>
      </div>
      <h2>{props.title}</h2>
      {props.hint && <p className="muted">{props.hint}</p>}
    </div>
  );
}
