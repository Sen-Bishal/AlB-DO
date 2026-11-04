import { action } from "albedo";

// R1a · a route restricted to roles. Its action inherits the requirement, which
// is only possible if the manifest records the roles beside the action ids.
export const auth = { role: ["payroll", "hr"] };

export const payroll_close = action(({ form }) =>
  append("guestbook", { author: form.author }),
);

export default function Payroll() {
  return <main>PAYROLL-LEAF</main>;
}
