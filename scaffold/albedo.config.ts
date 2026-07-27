// Phase N+ — file-based routing. `root` points at `src/` and the
// renderer discovers every `.tsx` under `routes/` automatically.
// `entry` is the default route's component file relative to `root`;
// other routes are discovered (and named) by their file path.
export default {
  contract_version: 1,
  root: "src",
  entry: "routes/index.tsx",
  server: { host: "127.0.0.1", port: 3000 },
  watch: { debounce_ms: 75, ignore: ["**/.git/**", "**/node_modules/**"] },
  hmr: { enabled: true, transport: "sse" },
  hot_set: [],
  static_slice: { enabled: true, opt_out: [] },

  // ── FORGE ────────────────────────────────────────────────────────
  //
  // THIS IS THE BACKEND. There is no server directory, no ORM, no
  // migration folder, no API layer. Declare the shape of the data and
  // ALBEDO emits the table, the query that materializes it, and the
  // seed rows — then keeps every connected client in sync with it.
  //
  // `id` is implicit on every collection (INTEGER PRIMARY KEY
  // AUTOINCREMENT) and is what live reconciliation keys on.
  //
  // The declared ordering decides where a new row lands, which decides
  // which opcode the change ships as on the wire. `guestbook` is
  // ordered ascending, so a new row lands at the TAIL and ships as a
  // keyed `SlotDelta` — O(|Δ|), not O(|list|), however long it gets.
  //
  // Add a collection here and it exists. That is the whole workflow.
  forge: {
    guestbook: {
      fields: { author: "text", message: "text" },
      seed: [
        { author: "ada", message: "first light" },
        { author: "alan", message: "the machine stirs" },
      ],
    },

    // `partition_by` splits one collection into many independent live
    // channels — one per distinct value of that field. `/room/a` and
    // `/room/b` read the same table and never see each other's rows,
    // and a write to one reaches only the tabs watching it.
    //
    // This is what makes per-room chat, per-user data and multi-tenancy
    // expressible. It also emits an index on (room, id), so reading one
    // room costs what the room costs, not what the table costs.
    //
    // See src/routes/room/[id].tsx for the read.
    messages: {
      fields: { room: "text", author: "text", body: "text" },
      partition_by: "room",
      seed: [
        { room: "lobby", author: "ada", body: "welcome to the lobby" },
        { room: "quiet", author: "alan", body: "a different room entirely" },
      ],
    },
  },
};
