import { useState } from "react";

// Stage 3 · module-level constants. `MAX` and `LABEL` are not props,
// not hook bindings, not setters — they live at module scope and
// must be visible to both the JSX render (`{LABEL}: {n}`) and the
// handler body (`Math.min(n + 1, MAX)`).
const MAX = 5;
const LABEL = "n";

export default function Capped() {
  const [n, setN] = useState(0);
  return (
    <button onClick={() => setN(Math.min(n + 1, MAX))}>{LABEL}: {n}</button>
  );
}
