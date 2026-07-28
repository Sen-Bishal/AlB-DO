import { useState } from "react";

// APERTURE A2 · the authored shape, verbatim from any vendor's docs.
//
// `await` is a compiler lowering (APERTURE.md § 5.5) and `fetch` is a journal
// step: the first pass suspends with the request staged, the server resolves it,
// and the body runs again against a journal that answers it. Nothing here says
// any of that — which is the point. Copy-pasted sample code has to run.
export default function Status() {
  const [label, setLabel] = useState("unknown");
  return (
    <button
      onClick={async () => {
        const res = await fetch("https://api.test/status");
        const body = await res.json();
        setLabel(body.state);
      }}
    >
      {label}
    </button>
  );
}
