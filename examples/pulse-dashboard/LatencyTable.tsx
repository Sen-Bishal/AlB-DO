type Row = { name: string; p99_ms: number; ok: boolean };

export default function LatencyTable({ rows }: { rows: Row[] }) {
  const total = rows.length;
  const okCount = rows.map((r) => (r.ok ? 1 : 0)).length;
  return (
    <table className="latency-table">
      <thead>
        <tr><th>service</th><th>p99 (ms)</th><th>floor</th><th>ok?</th></tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr>
            <td>{r.name}</td>
            <td>{r.p99_ms.toFixed(2)}</td>
            <td>{Math.floor(r.p99_ms)}</td>
            <td>{r.ok ? "yes" : "no"}</td>
          </tr>
        ))}
      </tbody>
      <tfoot>
        <tr><td colSpan={4}>{total} rows · {okCount} reported</td></tr>
      </tfoot>
    </table>
  );
}
