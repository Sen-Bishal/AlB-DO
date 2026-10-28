import Badge from "../components/Badge";
import Plan from "../components/Plan";

// The parent names the island and passes it props. This is the only place
// those props exist — the island is rendered later, standalone, from a
// module path, by which point `start={41}` is gone unless it was captured.
//
// `only` and `onboarding` are data that happen to begin with "on" — kept by the
// island and by `Plan`, a plain (Tier-A) child whose props take the evaluator's
// other path. No handler is passed here: one would make `Home` interactive and
// it would stop being the Tier-A parent these tests are about (`handlers.tsx`).
export default function Home() {
  return (
    <main className="page">
      <h1>island props</h1>
      <Badge start={41} label="clicks" only="solo" onboarding="step1" />
      <Plan only="annual" onboarding="trial" />
    </main>
  );
}
