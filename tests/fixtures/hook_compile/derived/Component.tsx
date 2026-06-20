import { useState } from "react";

// Derived-binding fixture: `{n}` is a bare read (SetTextRef), `{n * 2}` is a
// derived expression over the same slot. The client recomputes `n * 2` whenever
// `n` changes and re-applies it — no re-render, no round-trip.
export default function Derived() {
  const [n, setN] = useState(1);
  return (
    <div>
      <button id="b" onClick={() => setN(n + 1)}>
        inc
      </button>
      <span id="raw">{n}</span>
      <span id="dbl">{n * 2}</span>
    </div>
  );
}
