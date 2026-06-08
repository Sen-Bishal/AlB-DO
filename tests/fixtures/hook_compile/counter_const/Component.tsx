import { useState } from "react";

// Module-level constant referenced inside the click handler. Before module-const
// seeding landed, the QuickJS action path threw a `ReferenceError: STEP is not
// defined` here; the pure-Rust path resolved it via seed_env_with_module_constants.
const STEP = 5;

export default function Counter() {
  const [n, setN] = useState(0);
  return (
    <button onClick={() => setN(n + STEP)}>{n}</button>
  );
}
