import { useState } from "react";

export default function Counter() {
  // Tier B: useState introduces interactivity — AlBDO emits a small
  // hydration island so only this card boots on the client.
  const [count, setCount] = useState(0);

  return (
    <>
      <div className="card-head">
        <span className="tier-badge">
          <span>tier b</span>
        </span>
        <span className="card-meta">
          <span>island</span>
        </span>
      </div>
      <h3 className="card-title">Interactive island</h3>
      <p className="card-body">
        Hooks move this component into Tier B. AlBDO isolates it, ships the
        minimum JS to hydrate just this card, and keeps the rest static.
      </p>
      <div className="counter-action">
        <button
          type="button"
          className="counter-btn"
          onClick={() => setCount((value) => value + 1)}
        >
          + increment
        </button>
        <span className="counter-value">{count}</span>
      </div>
    </>
  );
}
