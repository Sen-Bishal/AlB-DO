import { useState } from "react";

// A second island, so the Tier-B parent case can be asserted without
// colliding with `Badge` — island props are keyed by component name, so
// reusing one island across two parents would make the assertion ambiguous.
export default function Ticker({ seed }) {
  const [n, setN] = useState(seed);
  return (
    <div className="ticker">
      <button onClick={() => setN(n + 1)}>tick</button>
      <span className="n">{n}</span>
    </div>
  );
}
