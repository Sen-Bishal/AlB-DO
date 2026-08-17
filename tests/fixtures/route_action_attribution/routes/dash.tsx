import { action } from "albedo";

// § 8.1.3's motivating shape: a route restricted by declaration, with an action
// of its own. The action inherits this gate — which is only possible if the
// manifest records that this route is where the action was written.
export const auth = "required";

export const dash_write = action(({ form }) =>
  append("guestbook", { author: form.author }),
);

export const dash_purge = action(() => remove("guestbook", 1));

export default function Dash() {
  return <main>DASH-LEAF</main>;
}
