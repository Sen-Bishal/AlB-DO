import { useState } from "react";

// A wrapper, a handler and a read — the smallest shape with real nesting, and
// the one that exposed the anchor-parity bug. The single-element fixtures beside
// this one cannot: with one element there is no ordering to get wrong, so both
// renderers agree by accident rather than by construction.
export default function NestedIsland() {
  const [label, setLabel] = useState("idle");
  return (
    <div className="island">
      <button type="button" onClick={() => setLabel("pressed")}>
        go
      </button>
      <span className="tally">{label}</span>
    </div>
  );
}
