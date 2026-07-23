import Hero from "../components/Hero";
import Counter from "../components/Counter";

// The landing route. Deliberately short — the starter should show you
// what the thing does, then get out of the way. Depth lives in the docs.
export default function Home() {
  return (
    <>
      <Hero />

      <ul className="points">
        <li className="point">
          <span className="point-n" aria-hidden="true">
            01
          </span>
          <p className="point-t">
            Components that don&rsquo;t move ship as markup. No script tag, no
            hydration, <em>nothing to boot.</em>
          </p>
        </li>
        <li className="point">
          <span className="point-n" aria-hidden="true">
            02
          </span>
          <p className="point-t">
            The ones that do move ship alone. Only that component reaches the
            browser — <em>not the page around it.</em>
          </p>
        </li>
        <li className="point">
          <span className="point-n" aria-hidden="true">
            03
          </span>
          <p className="point-t">
            Your database is a block in <code>albedo.config.ts</code>. Name a
            collection and the table, the query and the seed rows{" "}
            <em>are written for you.</em>
          </p>
        </li>
        <li className="point">
          <span className="point-n" aria-hidden="true">
            04
          </span>
          <p className="point-t">
            A write lands in every open tab. What crosses the wire is the row
            that changed — <em>not the list it belongs to.</em>
          </p>
        </li>
        <li className="point">
          <span className="point-n" aria-hidden="true">
            05
          </span>
          <p className="point-t">
            One binary runs all of it. <em>No Node, no bundler, no daemon.</em>
          </p>
        </li>
      </ul>

      <div className="actions">
        <a className="act act-metal" href="/guestbook">
          See the backend <span className="arrow">&rarr;</span>
        </a>
        <a
          className="act act-quiet"
          href="https://github.com/anthropic-ai/albedo"
          rel="noreferrer"
        >
          Read the docs <span className="arrow">&rarr;</span>
        </a>
      </div>

      <section className="plate">
        <Counter />
      </section>
    </>
  );
}
