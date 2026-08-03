import { useState } from "react";

// A Tier-C island that takes props. `start` seeds the hook, `label` is a
// static read — between them they cover the two ways a prop reaches the
// output: through `useState`'s initial (which the client thunk must also
// know) and straight into the markup.
export default function Badge({ start, label }) {
  const [count, setCount] = useState(start);
  return (
    <div className="badge">
      <button onClick={() => setCount(count + 1)}>bump</button>
      <span className="label">{label}</span>
      <span className="count">{count}</span>
    </div>
  );
}
