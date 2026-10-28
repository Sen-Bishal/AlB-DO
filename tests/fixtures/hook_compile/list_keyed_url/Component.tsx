import { useState } from "react";

// ALB-03 · a keyed list whose row carries a URL from data. The rows thunk is
// shipped to the browser and rebuilds rows from state, so the URL rule has to
// live in the thunk too — not only in the two server renderers that painted
// the first row.
export default function Links() {
  const [items, setItems] = useState([{ id: 1, url: "https://example.com/" }]);
  return (
    <section>
      <button onClick={() => setItems([...items, { id: 2, url: "javascript:alert(1)" }])}>
        add
      </button>
      <ul>
        {items.map((item) => (
          <li key={item.id}>
            <a href={item.url}>link</a>
          </li>
        ))}
      </ul>
    </section>
  );
}
