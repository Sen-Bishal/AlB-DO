// Object-valued `style` props. Found by pointing the conformance harness at
// excalidraw, where `DropdownMenuSeparator.tsx` rendered `style="{...json...}"`
// from the pure-Rust evaluator and `style="[object Object]"` from QuickJS —
// neither of which is what React emits, and the two did not even agree on how
// to be wrong.
//
// Each element pins one clause of React's rule:
//   separator  — the original repro: camelCase hyphenation, and SOURCE ORDER
//                (an alphabetized re-ordering would put backgroundColor first)
//   sized      — numbers take `px`, except zero and except the unitless set
//   prefixed   — vendor and custom-property spellings
//   sparse     — null / false / empty values drop their declaration entirely
export default function StyleObject() {
  return (
    <div>
      <div
        style={{
          height: "1px",
          backgroundColor: "var(--default-border-color)",
          margin: "6px 0",
          flex: "0 0 auto",
        }}
      />
      <p style={{ width: 10, marginTop: 0, flexGrow: 2, lineHeight: 1.5, zIndex: 3 }}>sized</p>
      <span style={{ WebkitLineClamp: 2, msFlexOrder: 1, "--brand": "#333" }}>prefixed</span>
      <em style={{ color: null, display: false, padding: "", border: "1px solid" }}>sparse</em>
    </div>
  );
}
