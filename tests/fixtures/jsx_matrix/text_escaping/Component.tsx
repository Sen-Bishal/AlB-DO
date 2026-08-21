// Markup-significant characters in an interpolated TEXT position.
//
// The 90-case conformance corpus had no case with one, which is why the
// evaluator emitted every expression child raw for as long as it did:
// `render_children` took an `escape_expr_children` flag that both call sites
// passed `false`, so the escaping branch was unreachable and the two renderers
// disagreed on every string containing `<`, `>` or `&` — silently, because
// nothing in the corpus contained one.
//
// The attribute is here on purpose. It always escaped correctly on both sides,
// so pinning the pair keeps the two rules distinguishable: text escapes `& < >`
// and leaves quotes alone, attributes additionally escape `"`. Over-escaping
// text to match the attribute rule is not a fix — it is a different divergence.
export default function Component() {
  const bio = "<script>alert(1)</script> & \"quoted\" 'single'";
  return <span title={bio}>{bio}</span>;
}
