import { action } from "albedo";

// Expression-bodied arrows evaluate `broadcast(...)` directly via
// `eval_expr`. Block-bodied variants (added for A1) are exercised below;
// `eval_body_stmts` now runs statement-position side effects too.

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

// A1 — block-bodied handlers now execute their statement-position side
// effects. `eval_body_stmts` evaluates a bare `Stmt::Expr` (the broadcast
// call) for effects, recurses into `if`/blocks, and rejects genuinely
// unsupported constructs loudly instead of silently dropping them.
export const block_increment = action(() => {
  broadcast("counter", (n) => n + 1);
});

export const block_with_if = action(() => {
  if (true) {
    broadcast("counter", (_) => 42);
  } else {
    broadcast("counter", (_) => -1);
  }
});

// A loop is a Tier-B/C construct the pure-Rust evaluator does not model;
// dispatching this handler must error loudly, not silently no-op.
export const unsupported_loop = action(() => {
  for (let i = 0; i < 3; i = i + 1) {
    broadcast("counter", (n) => n + 1);
  }
});

export default function Demo() {
  return <div>demo</div>;
}
