import { useState } from "react";

export default function Counter() {
  const [count, setCount] = useState(0);
  const [label, setLabel] = useState("idle");
  return (
    <article className="counter">
      <h3>{label} counter</h3>
      <p>value: <strong>{count}</strong></p>
      <button type="button" onClick={() => setCount(count + 1)}>+ increment</button>
    </article>
  );
}
