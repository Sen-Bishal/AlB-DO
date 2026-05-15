type Service = { name: string; p99_ms: number; ok: boolean };

export default function StatusPill({ svc }: { svc: Service }) {
  const tone = svc.ok ? "ok" : "warn";
  const ms = (svc.p99_ms as number).toFixed(1);
  return (
    <span className={`pill pill-${tone}`}>
      <strong>{svc.name}</strong>
      <span> {ms}ms</span>
    </span>
  );
}
