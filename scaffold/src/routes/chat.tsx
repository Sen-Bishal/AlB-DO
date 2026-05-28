import { action, useSharedSlot } from "albedo";

// `/chat` route — the live demo of AlBDO's broadcast substrate.
//
// `useSharedSlot("lobby:counter")` reads the topic's current value
// at render time and subscribes the session's WT patches lane for
// future writes. `broadcast(topic, updater)` inside the action body
// fans the write out to every subscriber over the same lane.
//
// Try it: open this page in two browser tabs. Click "bump" in
// either — both tabs update without polling.

// v1 interpreter note: action handler bodies + updater bodies are
// expression-form here so `eval_handler_body` resolves through
// `eval_expr` directly. Block-bodied closures with side effects
// require Stmt::Expr support which is queued for a follow-up.
export const bump_counter = action(() =>
  broadcast("lobby:counter", (n) => n + 1),
);

export default function ChatLobby() {
  const counter = useSharedSlot("lobby:counter");
  return (
    <section className="card tier-b">
      <div className="card-head">
        <span className="tier-badge">
          <span>tier b · broadcast</span>
        </span>
        <span className="card-meta">
          <span>WT patches lane</span>
        </span>
      </div>
      <h3 className="card-title">Server-pushed counter</h3>
      <p className="card-body">
        Open this route in two browser tabs. Clicking <em>bump</em> in either
        updates both — the action calls <code>broadcast()</code> and the
        live value flows over the WebTransport patches lane.
      </p>
      <div className="counter-action">
        <form action="action:bump_counter" method="POST">
          <button type="submit" className="counter-btn">
            + bump
          </button>
        </form>
        <span className="counter-value">{counter}</span>
      </div>
    </section>
  );
}
