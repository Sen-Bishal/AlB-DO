import { useState } from "react";

// Negative fixture for the keyed-lists rung: the per-item subtree is NOT static
// — each `<li>` carries its own `on*` handler. Such an item can't be regenerated
// as inert innerHTML (the handler would be lost), so the renderer must flag a
// structural fallback and `build_reactive_payload` must error, keeping the
// component on its correct A3 whole-component island.
export default function RemovableList() {
  const [items, setItems] = useState([{ label: "a" }, { label: "b" }]);
  return (
    <ul>
      {items.map((item) => (
        <li onClick={() => setItems(items.filter((x) => x !== item))}>
          {item.label}
        </li>
      ))}
    </ul>
  );
}
