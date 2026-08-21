// The same escaping contract as `jsx_matrix/text_escaping`, but with the value
// arriving as PROPS rather than as a local constant.
//
// This is the shape that makes unescaped text a security defect rather than a
// cosmetic one: props are where outside data enters a render. The conformance
// harness supplies a route's `params` this way, so a URL segment rendered as
// `{params.id}` reaches a text position by exactly this path — and the
// evaluator's output is baked into the manifest, with no client code that could
// correct it afterwards.
export default function EscapedPropsText(props) {
  return <span title={props.bio}>{props.bio}</span>;
}
