import Counter from "./Counter";
import StatusPill from "./StatusPill";
import LatencyTable from "./LatencyTable";

export default function App() {
  const iso = new Date(0).toISOString();
  const services = [
    { name: "edge",    p99_ms: 12.3456, ok: true },
    { name: "render",  p99_ms: 47.901,  ok: true },
    { name: "stitch",  p99_ms: 9.4242,  ok: false },
  ];
  return (
    <main className="pulse">
      <header className="pulse-head">
        <h1>pulse dashboard</h1>
        <p>boot at <time>{iso}</time></p>
      </header>
      <section className="pulse-counters">
        <Counter />
      </section>
      <section className="pulse-services">
        <h2>services ({services.length})</h2>
        <LatencyTable rows={services} />
        <ul className="pulse-pills">
          {services.map((svc) => <li><StatusPill svc={svc} /></li>)}
        </ul>
      </section>
    </main>
  );
}
