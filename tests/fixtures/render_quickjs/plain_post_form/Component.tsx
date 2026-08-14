// Plain `<form>`s — no `action:` sentinel — and which of them earn the hidden
// CSRF + return-path inputs. The rule lives in
// `transforms::form::plain_form_needs_hidden_inputs`; this fixture is what makes
// the two renderers agree about it, which is the property the served-markup
// contract exists to hold (the QuickJS path once emitted no CSRF input at all).
//
// Each form pins one clause:
//   signin   — same-origin POST: earns both inputs. This is the sign-in form
//              AUTH P2 needs, and before the rule generalized there was no
//              spelling of it that worked — every submit answered 403.
//   rooted   — same-origin POST at an app's own path: item 8's no-JS submit
//   search   — GET: no token, or it lands in the URL, the history and the log
//   offsite  — absolute action: no token, or this session's token is handed to
//              another origin on submit
//   protorel — `//host` is a URL, not a rooted path. The case that looks rooted.
export default function PlainPostForm() {
  return (
    <div>
      <form action="/_albedo/auth/password/login" method="POST">
        <input name="email" />
        <input name="password" type="password" />
      </form>
      <form action="/subscribe" method="post">
        <input name="email" />
      </form>
      <form action="/search" method="get">
        <input name="q" />
      </form>
      <form action="https://evil.example/collect" method="POST">
        <input name="loot" />
      </form>
      <form action="//evil.example/collect" method="POST">
        <input name="loot" />
      </form>
    </div>
  );
}
