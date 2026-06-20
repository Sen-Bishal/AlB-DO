import { useState, useMemo } from "react";

// useMemo-local resolution: `{doubled}` reads a local whose definition is a
// `useMemo(() => n * 2, [n])`. The derived-binding analysis substitutes the memo
// body, so `{doubled}` recomputes `n * 2` from `n`'s slot — no re-render.
export default function Memoized() {
  const [n, setN] = useState(1);
  const doubled = useMemo(() => n * 2, [n]);
  return (
    <div>
      <button id="b" onClick={() => setN(n + 1)}>
        inc
      </button>
      <span id="raw">{n}</span>
      <span id="dbl">{doubled}</span>
    </div>
  );
}
