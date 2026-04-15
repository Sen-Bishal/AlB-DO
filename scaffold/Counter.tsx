import { useState } from "react";

export default function Counter() {
  // Tier B: useState hook introduces local interactivity, so hydration is required.
  const [count, setCount] = useState(0);

  return (
    <section>
      <h2>Tier B - Hydrated Counter</h2>
      <p>
        Hooks move this component out of Tier A. AlBDO sends only the hydration
        payload needed for this island.
      </p>
      <button type="button" onClick={() => setCount((value) => value + 1)}>
        Count: {count}
      </button>
    </section>
  );
}
