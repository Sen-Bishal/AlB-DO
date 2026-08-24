// Boolean-valued props, which HTML spells in TWO unrelated ways.
//
// Found in real markup from `@radix-ui/react-slot` served by `albedo serve`:
// `aria-expanded={true}` shipped as the bare `aria-expanded`, which is the empty
// string — assistive technology reads that as *not expanded*. All three
// renderers had the same defect, and `aria-*` is how Radix wires every compound
// component's accessibility, so the whole shadcn/UI layer server-rendered with
// dead aria state.
//
// Each element pins one clause of the rule
// (`runtime::jsx_attributes::boolean_attribute_form`):
//   toggle     — the original repro side by side: `aria-expanded` takes the WORD,
//                `disabled` takes PRESENCE, from the same `true`
//   hidden/lit — `aria-hidden={false}` must render `aria-hidden="false"`, not
//                vanish: "not hidden" is a claim, and it is what stops an
//                ancestor's `aria-hidden="true"` being inherited over a subtree
//   field      — a renamed prop is judged by the attribute it BECOMES
//                (`defaultChecked` → `checked`, presence), beside an enumerated
//                one on the same tag
//   booleanish — the non-`aria-` enumerated set: HTML attributes that want the
//                word even though nothing about their name says so
//   icon       — the SVG half of that set, where `focusable={false}` is the
//                spelling every icon package emits
export default function BooleanAttributes() {
  const on = true;
  const off = false;
  return (
    <div>
      <button type="button" aria-expanded={on} aria-controls="panel" disabled={on}>
        toggle
      </button>
      <span aria-hidden={off}>read me</span>
      <span aria-hidden={on}>decorative</span>
      <input type="checkbox" defaultChecked={on} required={off} aria-checked={on} />
      <p contentEditable={off} draggable={on} spellCheck={off}>
        booleanish
      </p>
      <svg viewBox="0 0 8 8" focusable={off} aria-hidden={on}>
        <circle cx="4" cy="4" r="3" strokeWidth={2} />
      </svg>
    </div>
  );
}
