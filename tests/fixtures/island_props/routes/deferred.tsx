import Ticker from "../components/Ticker";

// An async server component — Tier B, rendered on a different path from the
// static Tier-A route. Its island must still receive what it was passed.
export default async function Deferred() {
  return (
    <section className="deferred">
      <Ticker seed={7} />
    </section>
  );
}
