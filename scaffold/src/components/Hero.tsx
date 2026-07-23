// Tier A — no hooks, no async, no side effects. This whole masthead
// ships as HTML. Nothing here boots on the client.
//
// The apostrophe in ALB'DO is the wordmark signature, so it carries the
// metal on its own (`.glyph`) rather than being set as plain type.
export default function Hero() {
  return (
    <header className="hero">
      <h1 className="wordmark">
        ALB<span className="glyph">&rsquo;</span>DO
      </h1>
      <p className="tagline">Evolution simplified</p>
      <hr className="rule" />
      <p className="lede">
        A compiler that reads your components and decides what each one
        actually costs — what ships as markup, what boots on the client, and
        what your database looks like.
      </p>
    </header>
  );
}
