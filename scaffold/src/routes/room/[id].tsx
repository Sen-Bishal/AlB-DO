import { action, useSharedSlot } from "albedo";
import { messages } from "albedo/forge";

// A room. Try /room/lobby and /room/quiet — same table, different data,
// and a message posted in one never appears in the other.
//
// The `[id]` in this file's name makes the route dynamic: whatever the URL
// puts there arrives as `params.id`.

// `room` is an ordinary field on the record. Because the collection declares
// `partition_by: "room"`, the server reads the key off the record and routes
// the change to that room's subscribers — you never say where it goes.
export const post_message = action(({ form }) =>
  append("messages", { room: form.room, author: form.author, body: form.body }),
);

export default function Room({ params }: { params: { id: string } }) {
  // The important line.
  //
  // `messages.where({ room: params.id })` reads one partition. Notice there
  // is no topic string anywhere — you cannot write one, and that is on
  // purpose: the compiler mints the channel identity, so two rooms can never
  // accidentally end up sharing one.
  //
  // Today the key can be a route param. Reading `user.id` here is a build
  // error that says so — it arrives with auth.
  //
  // No type annotation: the row shape comes from the `forge` block in
  // albedo.config.ts, so `row.body` autocompletes and a typo is a build error.
  const rows = useSharedSlot(messages.where({ room: params.id }));

  return (
    <main className="page">
      <h1 className="title">#{params.id}</h1>
      <p className="lede">
        One collection, one channel per room. Open <code>/room/lobby</code> and{" "}
        <code>/room/quiet</code> in two tabs and post in each.
      </p>

      {/* Same two rules as the guestbook: don't guard the .map(), and key
          the rows. The key is what lets a new message slide in without
          rebuilding the list. */}
      <ul className="entries">
        {rows.map((row) => (
          <li className="entry" key={row.id}>
            <strong>{row.author}</strong> {row.body}
          </li>
        ))}
      </ul>

      <form action="action:post_message" method="POST" className="row">
        {/* Carries the room into the write. */}
        <input type="hidden" name="room" value={params.id} />
        <input className="field" name="author" placeholder="your name" />
        <input className="field" name="body" placeholder="say something" />
        <button className="btn" type="submit">
          post
        </button>
      </form>
    </main>
  );
}
