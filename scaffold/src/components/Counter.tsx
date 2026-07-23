import { useState } from "react";

// `useState` plus a click handler makes this interactive, so the
// compiler moves it out of the static tier and it becomes the only
// thing on this page that boots on the client. The masthead, the list
// of points and the shell around it are markup and stay that way.
//
// Run `albedo build` to see which tier it actually landed in and what
// it cost — the compiler decides, not this comment.
export default function Counter() {
  const [count, setCount] = useState(0);

  return (
    <>
      <p className="plate-eyebrow">the only script on this page</p>
      <h2 className="plate-title">One button, one island</h2>
      <p className="plate-body">
        A hook moved this component out of the static tier, so its code — and
        nothing else from this page — was sent to your browser. Everything
        above arrived finished. Run <code>albedo build</code> to see the
        byte count it cost you.
      </p>
      <div className="island">
        <button
          type="button"
          className="submit submit-quiet"
          onClick={() => setCount(count + 1)}
        >
          press
        </button>
        <span className="tally">{count}</span>
      </div>
    </>
  );
}
