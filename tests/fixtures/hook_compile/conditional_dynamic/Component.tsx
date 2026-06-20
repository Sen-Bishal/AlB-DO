import { useState } from "react";

// Binding-mode fallback fixture: a slot-reactive conditional whose branch is
// NOT static — it reads state (`{count}`) inside the toggled subtree. Binding
// mode can't represent this fine-grained (the appearing subtree carries its own
// bindings), so the renderer flags a structural fallback and
// `build_reactive_payload` errors — the component then keeps its correct A3
// whole-component island.
export default function DynamicDisclosure() {
  const [open, setOpen] = useState(false);
  const [count, setCount] = useState(0);
  return (
    <section>
      <button onClick={() => setOpen(!open)}>toggle</button>
      <button onClick={() => setCount(count + 1)}>inc</button>
      {open && <p>{count}</p>}
    </section>
  );
}
