import { useState } from "react";

// Binding-mode fallback fixture: a handler that reads its DOM event argument
// (`e.target.value`). Binding mode's client thunk wires only `__state`, setters,
// and captured props — NOT the event — so this handler can't be lowered
// fine-grained. `build_reactive_payload` must decline (error) so the component
// falls back to its correct A3 whole-component island, where the real closure
// runs in the browser with the native event. Mirrors Halation's MarginNote.
export default function Composer() {
  const [count, setCount] = useState(0);
  return (
    <div>
      <textarea onInput={(e) => setCount(e.target.value.length)} />
      <span>{count}</span>
    </div>
  );
}
