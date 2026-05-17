import { useState } from "react";

// Stage 3 · forward-referenced consts (chained). `DOUBLE` references
// `BASE`, so the sequential-eval ordering matters: BASE must be
// resolved before DOUBLE.
const BASE = 7;
const DOUBLE = BASE + BASE;

export default function Chained() {
  const [n, setN] = useState(BASE);
  return (
    <button onClick={() => setN(DOUBLE)}>{n}</button>
  );
}
