export function EmptyState(props: { title: string; hint?: string }) {
  return (
    <div className="empty-state">
      <h2>{props.title}</h2>
      {props.hint && <p className="muted">{props.hint}</p>}
    </div>
  );
}
