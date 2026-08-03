import Badge from "../components/Badge";

// The parent names the island and passes it props. This is the only place
// those props exist — the island is rendered later, standalone, from a
// module path, by which point `start={41}` is gone unless it was captured.
export default function Home() {
  return (
    <main className="page">
      <h1>island props</h1>
      <Badge start={41} label="clicks" />
    </main>
  );
}
