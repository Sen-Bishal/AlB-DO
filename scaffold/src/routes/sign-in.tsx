// `/sign-in` — accounts, with no auth library and no third party.
//
// The `auth` block in albedo.config.ts declares one provider (`password`).
// From that declaration ALB'DO emits four tables — users, accounts, sessions,
// credentials — and mounts the endpoints these forms post to. There is no
// users table to design, no session middleware to install, no callback route
// to write, and no `next-auth`-shaped dependency to keep current.
//
// The thing worth reading the source for: **this whole page works with
// JavaScript switched off.**
//
// The forms post to real URLs — nothing calls `fetch`, no handler intercepts
// the submit, and the CSRF token and return path are stamped into every
// same-origin POST form by the renderer rather than by a script. The render is
// server-side too: this component runs on the request that asked for it and its
// markup arrives as markup, so turning JavaScript off changes nothing about
// what you see here or whether you can sign in.
//
// That is also why passwords exist at all. Passkeys are the better credential
// and the intended default, but `navigator.credentials` is a JavaScript API
// with no HTML fallback — password is what keeps the floor honest.
//
// Gate any route on a session with one line in that route's file:
//
//     export const auth = "required";
//
// A stranger who asks for it is refused before the render runs and sent here,
// because `login: "/sign-in"` in albedo.config.ts named this page. Nothing is
// inferred from "the route with a form on it" — a security-adjacent redirect
// target that moved when you added a form somewhere else would be a bad thing
// to own.
//
// ── `user` is a prop, and it is either a principal or null ───────────────
//
// Naming `user` is what asks for it. The compiler reads that off this file and
// records it, which has a second consequence worth knowing: **a component that
// reads `user` is rendered per request.** It cannot be baked at build time,
// because at build time there is no request and therefore nobody signed in.
// You do not opt into that and you cannot forget to.
//
// So an anonymous visitor and a signed-in one get different HTML from the same
// component, with no client-side "am I logged in yet" flash. Only `id` is
// exposed: profile fields are rows in `albedo_users`, read like any other row,
// so mentioning `user` never silently joins a table you did not ask for.
//
// The same prop is what unlocks per-person data:
//
//     const notes = useSharedSlot(notes.where({ owner: user.id }));
//
// That read *is* the policy. There is no channel name you could write, so
// there is nothing to author beside the query and nothing to keep in step.

export default function SignIn({ user }: { user: { id: string } | null }) {
  if (user) {
    return (
      <main className="page">
        <section className="plate">
          <p className="plate-eyebrow">auth &middot; session</p>
          <h1 className="plate-title">Signed in</h1>
          <p className="plate-body">
            Your principal is <code className="mono">{user.id}</code>. It is
            ours, not the provider&rsquo;s &mdash; which is what lets one person
            attach a second provider later and stay the same account.
          </p>

          {/* Signing out revokes THIS session, not every session: logging out
              on a laptop must not sign you out on a phone. */}
          <form action="/_albedo/auth/logout" method="POST" className="row">
            <button className="submit" type="submit">
              sign out
            </button>
          </form>

          <p className="plate-note">
            The session is a row in <code>albedo_sessions</code>, partitioned by
            principal &mdash; so revoking one is a delete, and it travels the
            same delta path your own data does.
          </p>
        </section>
      </main>
    );
  }

  return (
    <main className="page">
      <section className="plate">
        <p className="plate-eyebrow">auth &middot; password</p>
        <h1 className="plate-title">Sign in</h1>
        <p className="plate-body">
          Declared as <code>{`providers: { password: {} }`}</code>. The tables,
          the endpoints and the session cookie come from that one line.
        </p>

        <form
          action="/_albedo/auth/password/login"
          method="POST"
          className="row"
        >
          <input
            className="field"
            name="email"
            type="email"
            placeholder="email"
            autocomplete="username"
          />
          <input
            className="field"
            name="password"
            type="password"
            placeholder="password"
            autocomplete="current-password"
          />
          <button className="submit" type="submit">
            sign in
          </button>
        </form>

        <p className="plate-note">No account yet? Make one.</p>

        <form
          action="/_albedo/auth/password/register"
          method="POST"
          className="row"
        >
          <input
            className="field"
            name="email"
            type="email"
            placeholder="email"
            autocomplete="username"
          />
          <input
            className="field"
            name="password"
            type="password"
            placeholder="password"
            autocomplete="new-password"
          />
          <button className="submit submit-quiet" type="submit">
            sign up
          </button>
        </form>

        <p className="plate-note">
          Passwords are stored as argon2id hashes, and the rate limiter treats a
          credential attempt as its own class &mdash; a wrong guess costs the
          guesser per account, not just per address.
        </p>
      </section>
    </main>
  );
}
