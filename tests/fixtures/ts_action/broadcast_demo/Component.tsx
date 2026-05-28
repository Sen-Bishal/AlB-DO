import { action } from "albedo";

// Expression-bodied arrows so the Phase J/K interpreter evaluates
// `broadcast(...)` directly via `eval_expr` (block bodies only run
// `Stmt::Return` / `Stmt::Decl(Var)` in v1 — Stream C.2 doesn't
// extend that surface).

export const set_counter_to_seven = action(() => broadcast("counter", (_) => 7));

export const increment_counter = action(() => broadcast("counter", (n) => n + 1));

export const reset_counter = action(() => broadcast("counter", (_) => 0));

// Updater that returns a structured value — proves the JSON
// round-trip carries arrays/objects unchanged (i.e. the encode is
// `serde_json::to_vec`, not a string coercion). Spread / concat
// aren't exercised here because the Phase J/K interpreter doesn't
// yet expand array spreads inside literal expressions.
export const replace_log_with_two_items = action(() =>
  broadcast("chat:lobby", (_) => ["alpha", "beta"]));

export default function Demo() {
  return <div>demo</div>;
}
