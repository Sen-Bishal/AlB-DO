import { action } from "albedo";

// A public route that declares an action. It is the control: the gate must
// separate this from `dash.tsx` on the declaration alone, and both must reach
// the manifest attributed to their own route.
export const sign_guestbook = action(({ form }) =>
  append("guestbook", { author: form.author }),
);

export default function Home() {
  return <main>HOME-LEAF</main>;
}
