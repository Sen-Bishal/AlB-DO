import { Slot } from "@radix-ui/react-slot";

// No hook, no handler, no effect — every signal the tiering cascade reads says
// "Tier A". The one thing it cannot say for itself is that the renderer which
// bakes Tier A has no `node_modules` to resolve `@radix-ui/react-slot` against.
export default function Wrapper() {
  return (
    <div className="wrapper">
      <Slot className="added">
        <button className="mine">Open</button>
      </Slot>
    </div>
  );
}
