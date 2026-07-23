// Root layout — wraps every route under `routes/`.
//
// `<children />` marks where the leaf route's HTML is substituted at
// build time. Nested directories can declare their own `layout.tsx`;
// they compose root → leaf.

export default function RootLayout() {
  return (
    <div className="shell">
      <nav className="masthead">
        <a className="mark" href="/">
          ALB<span className="glyph">&rsquo;</span>DO
        </a>
        <span className="pulse">live</span>
      </nav>

      <children />

      <footer className="colophon">
        <span>src/routes/index.tsx</span>
        <span>
          <a href="https://github.com/anthropic-ai/albedo" rel="noreferrer">
            docs
          </a>
        </span>
      </footer>
    </div>
  );
}
