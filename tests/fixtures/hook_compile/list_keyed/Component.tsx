import { useState } from "react";

// Binding-mode KEYED-lists fixture: same shape as `list/`, but each item root
// carries an explicit `key={item.id}`. That selects the delta-sink lane — the
// SSR rows are stamped with `data-albedo-key`, the wrapper is a `SetListRef`
// anchor, and appending an item drives keyed reconciliation (`ReconcileList`)
// instead of an `innerHTML` rebuild. So the payload carries a `lists` binding,
// not a `derived` html binding.
export default function TodoList() {
  const [items, setItems] = useState([
    { id: 1, label: "a" },
    { id: 2, label: "b" },
  ]);
  return (
    <section>
      <button onClick={() => setItems([...items, { id: 3, label: "c" }])}>
        add
      </button>
      <ul>
        {items.map((item) => (
          <li key={item.id} className="row">
            {item.label}
          </li>
        ))}
      </ul>
    </section>
  );
}
