import Hero from "./components/Hero";
import Counter from "./components/Counter";
import LiveFeed from "./components/LiveFeed";

export default function App() {
  return (
    <div className="app-shell">
      <nav className="nav">
        <div className="brand">
          <span className="brand-mark" aria-hidden="true" />
          <span>AlBDO</span>
        </div>
        <span className="nav-pill">
          <span className="dot" />
          dev server · live
        </span>
      </nav>

      <Hero />

      <div className="section-heading">
        <h2>Three tiers, one render loop</h2>
        <span className="tag">auto-classified</span>
      </div>

      <section className="tier-grid">
        <article className="card tier-a">
          <div className="card-head">
            <span className="tier-badge">
              <span>tier a</span>
            </span>
            <span className="card-meta">
              <span>zero js</span>
            </span>
          </div>
          <h3 className="card-title">Static shell</h3>
          <p className="card-body">
            Pure components ship as HTML. No hydration payload, no runtime
            cost, no hydration mismatch. The fastest tier in the system.
          </p>
          <div className="card-meta">
            <span>
              payload <span className="kv">0 kB</span>
            </span>
            <span>
              hydrate <span className="kv">never</span>
            </span>
          </div>
        </article>

        <article className="card tier-b">
          <Counter />
        </article>

        <article className="card tier-c">
          <LiveFeed />
        </article>
      </section>

      <div className="section-heading">
        <h2>What makes it fast</h2>
        <span className="tag">design notes</span>
      </div>

      <section className="tier-grid">
        <article className="card">
          <h3 className="card-title">Rust + QuickJS SSR</h3>
          <p className="card-body">
            No Node.js on the critical path. Components compile through SWC
            and render via an embedded QuickJS engine, all in one binary.
          </p>
        </article>
        <article className="card">
          <h3 className="card-title">WebTransport streaming</h3>
          <p className="card-body">
            Four parallel slots for control, shell, patches and prefetch hints
            — shell arrives first, interactivity lands as soon as it&apos;s ready.
          </p>
        </article>
        <article className="card">
          <h3 className="card-title">Island hydration</h3>
          <p className="card-body">
            Only the interactive pieces boot on the client. The rest stays
            static markup, keeping bundles tiny and time-to-interactive low.
          </p>
        </article>
      </section>

      <footer className="footer">
        <span>AlBDO starter · edit src/App.tsx to begin</span>
        <span>
          <a href="https://github.com/anthropic-ai/albedo" rel="noreferrer">
            docs →
          </a>
        </span>
      </footer>
    </div>
  );
}
