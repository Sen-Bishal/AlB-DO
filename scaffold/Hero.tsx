export default function Hero() {
  // Tier A: no hooks, no async IO, no side effects. This compiles to static HTML.
  return (
    <section>
      <h2>Tier A - Static Hero</h2>
      <p>
        This component is intentionally pure. AlBDO can emit it as server HTML
        with zero client hydration JavaScript.
      </p>
    </section>
  );
}
