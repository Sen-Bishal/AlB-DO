import { useState } from "react";

export default function Stepper({ step }: { step: number }) {
  // Stage 2 · the handler closes over `step` (a component prop) and
  // uses it in the slot-write expression. The compiler must capture
  // `step` at render time so the server-side handler dispatch can
  // resolve it when bakabox POSTs the action.
  const [n, setN] = useState(0);
  return (
    <button onClick={() => setN(n + step)}>{n}</button>
  );
}
