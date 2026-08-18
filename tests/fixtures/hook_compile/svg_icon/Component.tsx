import { useState } from "react";

// Every attribute here is one the browser IGNORES if its case is wrong: an SVG
// presentation attribute must be hyphenated (`stroke-width`), while `viewBox`
// must keep its camelCase. Both renderers have to spell them the same way or
// hydration replaces the adopted node instead of keeping it.
export default function SvgIcon() {
  const [on, setOn] = useState(false);
  return (
    <button onClick={() => setOn(!on)} className="icon-button">
      <svg
        viewBox="0 0 24 24"
        preserveAspectRatio="xMidYMid meet"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
        fillRule="evenodd"
        clipRule="evenodd"
        pointerEvents="none"
      >
        <polyline points="20 6 9 17 4 12" strokeDasharray="4 2" />
        <text textAnchor="middle" fontFamily="serif" fontSize="10" letterSpacing="1">
          ok
        </text>
      </svg>
    </button>
  );
}
