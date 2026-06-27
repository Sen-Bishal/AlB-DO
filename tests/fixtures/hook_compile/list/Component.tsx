import { useState } from "react";

// Binding-mode keyed-lists fixture: a `useState` array drives a `.map()` of a
// STATIC per-item subtree (`<li>{item.label}</li>` — host element, no handlers,
// item-only `{expr}` hole). Phase K wraps the list position in a
// `display:contents` span and records a list binding; the client recomputes the
// whole list's innerHTML from local state when the button appends an item —
// data-driven structural reactivity, no component hydration, no round-trip.
export default function TodoList() {
  const [items, setItems] = useState([{ label: "a" }, { label: "b" }]);
  return (
    <section>
      <button onClick={() => setItems([...items, { label: "c" }])}>add</button>
      <ul>
        {items.map((item) => (
          <li className="row">{item.label}</li>
        ))}
      </ul>
    </section>
  );
}
