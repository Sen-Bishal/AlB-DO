// File-based routing. `root` points at `src/` and the
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

  // ── FORGE ────────────────────────────────────────────────────────
  //
  // THIS IS THE BACKEND. There is no server directory, no ORM, no
  // migration folder, no API layer. Declare the shape of the data and
  // ALBEDO emits the table and the query that materializes it — then keeps
  // every connected client in sync with it.
  //
  // A collection may also declare `seed: [ … ]` — rows written once, into an
  // empty table, on first boot. Use it for reference data your app needs
  // (plans, categories); these collections start empty and fill as people
  // post.
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
  //
  // Editing one that already holds rows works too: add a NULLABLE field and
  // ALBEDO alters the table on the next boot and says so. Anything else — a
  // drop, a rename, a type change, or a new REQUIRED field — refuses to start
  // and names the field, because there is no value it could invent for the rows
  // written before it. A refusal never touches your data.
  //
  // A field is `text`, `int`, `real`, `bool` or `timestamp`, with a trailing
  // `?` for nullable (`nickname: "text?"`). These decide the shape your rows
  // arrive in: a `bool` is `true`, never `1`; a `timestamp` is epoch
  // milliseconds, so `new Date(row.posted_at)` works with no conversion.
  //
  // Anyone may append. Only the signed-in user who added a row may `update`
  // or `remove` it — the server records who ran each append, so there is no
  // owner column to declare. `edit: "anyone"` opens a collection's rows to
  // every visitor; say it only when that is what you mean.
  forge: {
    guestbook: {
      fields: { author: "text", message: "text" },
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
    },
  },

  // ── AUTH ─────────────────────────────────────────────────────────
  //
  // Accounts, from a declaration. This block emits four tables
  // (`albedo_users`, `albedo_accounts`, `albedo_sessions`,
  // `albedo_credentials`), mounts the sign-in endpoints, and puts a `user`
  // prop on every render — either a principal or null. There is no auth
  // library to install and no users table to design.
  //
  // `password` is a known provider, so it infers what it is, as do `github`,
  // `google`, `discord` and `magiclink`. Anything else has to say what it is
  // (`kind: "oauth"` or `kind: "oidc"`) — guessing at an unknown name has a
  // silently wrong answer.
  //
  // See src/routes/sign-in.tsx. Gate any route on a session with
  // `export const auth = "required"` in that route's file; a stranger who
  // asks for it is sent to `login` below.
  //
  // `albedo_` is a reserved prefix. A collection declared under it above
  // is refused at boot — otherwise an app could replace the session table,
  // or read every user's row as one live topic.
  auth: {
    session: { ttl: "30d" },
    providers: {
      password: {},
    },
    login: "/sign-in",
  },

  // ── EMAIL ────────────────────────────────────────────────────────
  //
  // Uncomment to send mail. `sendEmail({ to, subject, text })` then works in
  // any action or job, and the password provider above can take
  // `reset: "email"` (a forgot-password flow) and `verify: "required"`
  // (confirm the address before first sign-in). Add `magiclink: {}` under
  // `providers` for passwordless sign-in.
  //
  // `provider: "log"` sends nothing: each message lands in
  // `.albedo/outbox/` as an .eml file, and `albedo dev` prints it. For real
  // mail use "resend", "postmark", "sendgrid" or "mailgun" with
  // `apiKey: { env: "RESEND_API_KEY" }`. Links in mail point at
  // ALBEDO_PUBLIC_ORIGIN in production — never at a request's Host header.
  //
  // The framework's own emails (reset, confirm, sign-in link, sign-up notice)
  // can be replaced field by field under `templates` — passwordReset,
  // verifyEmail, magicLink, alreadyRegistered — using {{app}}, {{link}} and
  // {{expires}}. The build refuses a body without {{link}} or a subject with it.
  //
  // Check a real provider before users depend on it:
  //   albedo email test you@example.com
  //
  // email: {
  //   from: "My App <no-reply@example.com>",
  //   provider: "log",
  //   // templates: { magicLink: { subject: "Your {{app}} sign-in link" } },
  // },
};
