import { action, useSharedSlot } from "albedo";

// `/guestbook` — the whole point of ALB'DO in one file.
//
// There is no server directory, no ORM, no API route and no migration
// folder behind this page. The `forge` block in `albedo.config.ts`
// names a collection; the table, the query that materializes it and the
// seed rows are emitted from that. This file reads it and writes to it.
//
// Open this route in two tabs and sign it in one. The other updates
// without polling and without rebuilding the list — untouched rows keep
// their DOM nodes, which is why focus, selection and scroll survive.
//
// However many tabs you open, the browser holds ONE connection to the
// server — tabs share it and subscribe by route (the PHOSPHOR lane), so
// a stack of tabs costs what one costs.

// One row of the `guestbook` collection. `id` is assigned by the
// substrate (the key column is `INTEGER PRIMARY KEY AUTOINCREMENT`, so
// never pass it to `append`); `author` and `message` are the fields the
// `forge` block declares.
//
// Today you write this shape and keep it agreeing with the config by
// hand. It is the one piece of duplication left in this file, and it is
// the seam a compiler that reads the `forge` block would close.
interface Entry {
  id: number;
  author: string;
  message: string;
}

// `append(collection, record)` records a durable write. The server
// applies it after this body returns, rematerializes the collection and
// fans the change out. `form` is ambient here and carries the submitted
// fields.
export const sign_guestbook = action(({ form }) =>
  append("guestbook", { author: form.author, message: form.message }),
);

// `remove(collection, key)` retracts the row identified by `key`.
export const remove_entry = action(({ form }) => remove("guestbook", form.id));

export default function Guestbook() {
  // The topic is materialized from forge.db at boot and seeded before
  // any listener binds, so it already holds real rows when a request
  // arrives. The type argument is what makes `entry` below a real
  // `Entry` instead of `unknown` — without it, `strict` mode rejects the
  // `.map()` and every field read inside it.
  const entries = useSharedSlot<Entry[]>("guestbook");

  return (
    <section className="plate">
      <p className="plate-eyebrow">forge &middot; collection</p>
      <h1 className="plate-title">Guestbook</h1>
      <p className="plate-body">
        Declared as <code>{`{ author: "text", message: "text" }`}</code> and
        ordered ascending, so a new row lands at the tail. What crosses the
        wire on a write is that row — the cost is the size of the change, not
        the size of the list.
      </p>

      {/*
        `entries.map()` is deliberately UNGUARDED. Writing
        `(entries || []).map(...)` hides a failure twice over: it swallows
        the error, and it stops the compiler seeing a bare slot identifier,
        which silently drops the reactive binding and leaves you with a list
        that renders once and never updates again. If the topic doesn't reach
        this render, this route should fail loudly instead of rendering a
        quietly empty list.
      */}
      <ul className="ledger">
        {entries.map((entry) => (
          <li className="row" key={entry.id}>
            <span className="row-n">{entry.id}</span>
            <span>
              <span className="row-who">{entry.author}</span>
              <span className="row-sep">&mdash;</span>
              <span className="row-what">{entry.message}</span>
            </span>
          </li>
        ))}
      </ul>

      <form action="action:sign_guestbook" method="POST" className="entry">
        <input className="field" name="author" placeholder="name" />
        <input className="field" name="message" placeholder="say something" />
        <button className="submit" type="submit">
          sign
        </button>
      </form>

      <form action="action:remove_entry" method="POST" className="entry">
        <input
          className="field field-narrow"
          name="id"
          placeholder="row to drop"
        />
        <button className="submit submit-quiet" type="submit">
          remove
        </button>
      </form>

      <p className="plate-note">
        Run <code>albedo serve</code>, open this page in a second tab, and sign
        it here. Both update. Neither one asked.
      </p>
    </section>
  );
}
