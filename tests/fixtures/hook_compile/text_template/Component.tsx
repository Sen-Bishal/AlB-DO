import { useState } from "react";

// The reported shape: a reactive read sharing its element with literal text.
// A binding that addresses the <span> owns its WHOLE text, so updating it from
// the slot alone deletes `total: `. The element's text is a template and has to
// be rebuilt as one.
export default function Total() {
  const [count, setCount] = useState(0);
  return (
    <div>
      <button onClick={() => setCount(count + 1)}>bump</button>
      <span className="tally">total: {count}</span>
      <span className="solo">{count}</span>
    </div>
  );
}
