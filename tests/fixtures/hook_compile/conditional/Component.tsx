import { useState } from "react";

// Binding-mode conditionals fixture: a boolean `useState` gates a STATIC
// subtree via `{open && <p className="panel">…}`. Phase K wraps the conditional
// position in a `display:contents` span and records a conditional binding; the
// client recomputes `open` locally when the button flips it and swaps the
// wrapper's innerHTML between the branch HTML and empty — fine-grained
// structural reactivity, no component hydration, no round-trip.
export default function Disclosure() {
  const [open, setOpen] = useState(false);
  return (
    <section>
      <button onClick={() => setOpen(!open)}>toggle</button>
      {open && <p className="panel">Now you see me</p>}
    </section>
  );
}
