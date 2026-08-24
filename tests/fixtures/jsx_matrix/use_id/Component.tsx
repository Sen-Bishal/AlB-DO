// `useId` across a component boundary — TODO.md 9.2.
//
// The point of the nesting is the ORDER. `useId` is only useful if the server
// and the client agree on which call gets which number, and the property that
// makes that true is that components are invoked parent-first, depth-first on
// both sides. A single flat component would pass without exercising it.
//
// Radix wires `aria-controls`/`aria-labelledby` between a trigger and its
// content out of exactly this shape, so a divergence here is a divergence in
// every shadcn primitive.
import { useId } from "react";

function Field() {
  const id = useId();
  return (
    <label htmlFor={id}>
      <input id={id} />
    </label>
  );
}

export default function Component() {
  const outer = useId();
  return (
    <div id={outer}>
      <Field />
      <Field />
    </div>
  );
}
