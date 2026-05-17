import { useState } from "react";

export default function Greeter({ initial, exclaim }: { initial: string; exclaim: string }) {
  // Stage 2 · two captured props of different shapes. `initial` seeds
  // the useState; `exclaim` is captured by the handler. The compiler
  // should snapshot BOTH props at render time so the handler can
  // build "alice!" or "alice???" depending on what was rendered.
  const [name, setName] = useState(initial);
  return (
    <button onClick={() => setName(name + exclaim)}>{name}</button>
  );
}
