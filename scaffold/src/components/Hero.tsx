export default function Hero() {
  // Tier A: no hooks, no async IO, no side effects — ships as static HTML.
  return (
    <header className="hero">
      <span className="hero-eyebrow">
        <span>●</span> v0.1 · tiered rendering
      </span>
      <h1 className="hero-title">
        Ship the web at the speed of <span className="accent">Rust</span>.
      </h1>
      <p className="hero-sub">
        AlBDO is a JSX compiler and render loop that sorts every component
        into one of three tiers — static, island, or streamed — and ships
        only what each tier actually needs.
      </p>
      <div className="hero-actions">
        <a className="btn btn-primary" href="#get-started">
          Open src/routes/index.tsx
        </a>
        <a className="btn btn-ghost" href="/chat">
          Try broadcast →
        </a>
      </div>
    </header>
  );
}
