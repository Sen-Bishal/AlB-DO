import { useState } from "react";

// Binding-mode attr fixture: a `{slot}` read in an attribute position
// (`className={cls}`). Phase K emits a SetAttrRef bound to `class` (the HTML
// name); the client handler flips the slot and the bound attribute re-applies.
export default function AttrToggle() {
  const [cls, setCls] = useState("off");
  return (
    <button className={cls} onClick={() => setCls("on")}>
      toggle
    </button>
  );
}
