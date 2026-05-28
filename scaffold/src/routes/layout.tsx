// Root layout — wraps every route under `routes/`.
//
// The `<children />` intrinsic marks where the leaf route's HTML
// gets substituted at build time. Add nav, header, footer, or any
// shell that should appear on every page. Nested directories can
// declare their own `layout.tsx`; AlBDO composes them root → leaf.

export default function RootLayout() {
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

      <children />

      <footer className="footer">
        <span>AlBDO starter · edit src/routes/index.tsx to begin</span>
        <span>
          <a href="https://github.com/anthropic-ai/albedo" rel="noreferrer">
            docs →
          </a>
        </span>
      </footer>
    </div>
  );
}
