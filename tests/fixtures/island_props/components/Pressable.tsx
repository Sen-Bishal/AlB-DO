import { useState } from "react";

// A third island, used by one route only, so the handler case can be asserted
// without colliding with `Badge` or `Ticker` — island props are keyed by
// component name.
export default function Pressable({ step }) {
  const [n, setN] = useState(0);
  return (
    <div className="pressable">
      <button onClick={() => setN(n + step)}>press</button>
      <span className="n">{n}</span>
    </div>
  );
}
