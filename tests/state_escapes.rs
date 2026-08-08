//! TODO #1 item 4.9 **T1** · state ownership decides Tier B vs Tier C.
//!
//! The rule: **does this component's state escape the client?** If it does —
//! a `useSharedSlot` topic, a FORGE write, a server `action`, a network call —
//! the *server* owns it, its updates already ride the wire as opcodes, and the
//! component ships no code. If it escapes nothing, an island is the honest
//! answer and Tier C is **correct, not a downgrade**.
//!
//! 🔑 **Why this signal had to exist.** The bit it sits beside,
//! `is_client_interactive`, is true for **534 of 536** interactive components in
//! the real-world corpus (`development-plan/TIER_DISTRIBUTION.md`) — a lever
//! whose negative case fires twice in 1,398 components is a constant. It was
//! deciding B vs C for 39.7% of all classifications by accident.
//!
//! ⚠️ **These tests pin the direction of the unknown case too** (see
//! `an_unrecognised_handler_stays_tier_c`). `state_escapes == false` means "not
//! proven to escape", never "proven local" — a wrong Tier C still works, a
//! wrong Tier B ships a binding for state the wire cannot drive.

use dom_render_compiler::effects::{
    decide_tier_and_hydration, TieringDecision, TieringInputs, TieringReason,
};
use dom_render_compiler::manifest::schema::{HydrationMode, Tier};
use dom_render_compiler::parser::ComponentParser;

fn inputs() -> TieringInputs {
    TieringInputs {
        tier_a_inline_max_bytes: 8 * 1024,
        tier_c_split_min_bytes: 40 * 1024,
        tier_b_mode: HydrationMode::OnIdle,
        tier_c_mode: HydrationMode::OnVisible,
    }
}

/// Parse one component and run the **production** decision over it, so these
/// assertions cannot drift from what `albedo build` prints.
fn decide(source: &str) -> TieringDecision {
    let parsed = ComponentParser::new()
        .parse_source(source, "Probe.tsx")
        .expect("parse");
    let c = parsed.first().expect("one component");
    decide_tier_and_hydration(
        c.effect_profile,
        c.is_interactive,
        c.is_client_interactive,
        c.state_escapes,
        false,
        c.estimated_size as u64,
        inputs(),
    )
}

/// **The case T1 exists for**, and the one the old cascade got wrong.
///
/// A shared counter with a click handler is client-satisfiable *and*
/// server-owned. The old rule saw only the first fact and shipped an island for
/// state the wire was already carrying.
#[test]
fn a_shared_slot_with_a_handler_is_tier_b_not_an_island() {
    let decision = decide(
        r#"
        import { useSharedSlot } from "albedo";
        export default function Tally() {
          const total = useSharedSlot("lobby:total");
          return <button onClick={() => bump()}>{total}</button>;
        }
        "#,
    );
    assert_eq!(
        decision.tier,
        Tier::B,
        "shared state is server-owned — it must not ship an island"
    );
    assert_eq!(decision.reason, TieringReason::ServerOwnedState);
}

/// The § 2c component. **Tier C is the correct answer**, and this test exists so
/// nobody "fixes" it back to B: there is no round trip to hang a delta on, and
/// driving a purely local click through the server buys a network hop and
/// nothing else.
#[test]
fn a_purely_local_counter_stays_tier_c() {
    let decision = decide(
        r#"
        import { useState } from "react";
        export default function Counter() {
          const [count, setCount] = useState(0);
          return <button onClick={() => setCount(count + 1)}>{count}</button>;
        }
        "#,
    );
    assert_eq!(decision.tier, Tier::C);
    assert_eq!(decision.reason, TieringReason::HookDrivenHydration);
}

/// A FORGE write inside a handler is state escaping to persistence.
///
/// 🪤 The effect-profile walk deliberately does **not** descend into handler
/// closures (a `fetch` in `onClick` is not a render-time io boundary). Escape
/// analysis must, because escape is a property of the transition, not of when it
/// runs. This test is what holds those two traversals apart.
#[test]
fn a_forge_write_inside_a_handler_escapes() {
    let decision = decide(
        r#"
        import { useState } from "react";
        export default function Composer() {
          const [draft, setDraft] = useState("");
          return <button onClick={() => append("posts", { body: draft })}>send</button>;
        }
        "#,
    );
    assert_eq!(decision.tier, Tier::B);
    assert_eq!(decision.reason, TieringReason::ServerOwnedState);
}

/// `onClick={handleAdd}` where the escape is one hop away. This indirection is
/// the common idiom, not an edge case, so it rides the same local-definition
/// fixpoint `is_client_interactive` already uses.
#[test]
fn an_escape_reached_through_a_local_definition_still_escapes() {
    let decision = decide(
        r#"
        import { useState } from "react";
        export default function Composer() {
          const [draft, setDraft] = useState("");
          const handleAdd = () => append("posts", { body: draft });
          return <button onClick={handleAdd}>send</button>;
        }
        "#,
    );
    assert_eq!(decision.tier, Tier::B, "escape is transitive through locals");
    assert_eq!(decision.reason, TieringReason::ServerOwnedState);
}

/// 🔴 **Escaping state must NOT pull a `useEffect` component back to Tier B.**
///
/// An effect body runs in the browser no matter who owns the state — it wires
/// listeners, measures the DOM, subscribes. A binding cannot express that, so
/// `side_effects` stays ahead of ownership in the cascade. This is the one
/// interaction most likely to be "simplified" wrongly later.
#[test]
fn a_side_effect_stays_tier_c_even_when_state_escapes() {
    let decision = decide(
        r#"
        import { useEffect } from "react";
        import { useSharedSlot } from "albedo";
        export default function Ticker() {
          const total = useSharedSlot("lobby:total");
          useEffect(() => { document.title = String(total); }, [total]);
          return <span>{total}</span>;
        }
        "#,
    );
    assert_eq!(
        decision.tier,
        Tier::C,
        "an effect body needs the client regardless of who owns the state"
    );
    assert_eq!(decision.reason, TieringReason::SideEffectBoundary);
}

/// **The conservative default, pinned.** A handler calling something the
/// analysis has never heard of is *not proven* to escape, so it keeps Tier C.
///
/// This is the direction that matters: a wrong Tier C still works (an island is
/// the general fallback), while a wrong Tier B would ship a binding for state
/// the wire cannot drive. Unknown rounds toward the top of the lattice.
#[test]
fn an_unrecognised_handler_stays_tier_c() {
    let decision = decide(
        r#"
        import { useState } from "react";
        export default function Widget() {
          const [n, setN] = useState(0);
          return <button onClick={() => mysteryLibrary.mutate(n)}>go</button>;
        }
        "#,
    );
    assert_eq!(
        decision.tier,
        Tier::C,
        "not proven to escape must mean Tier C, never Tier B"
    );
    assert_ne!(decision.reason, TieringReason::ServerOwnedState);
}

/// 🔴 **A locally-bound name shadows ALBEDO's ambient global.**
///
/// `react-hook-form`'s `useFieldArray` hands you `append`/`remove`/`update` for
/// purely client-side form-array state. Reading those as FORGE writes would
/// ship a Tier-B binding for state the wire cannot drive — a broken component,
/// in the one direction this analysis must never round toward.
///
/// 🪤 Found by measurement, not by review: before the shadow check, **6 of
/// 1,398 real-world components were misclassified, every one of them a
/// `useFieldArray` destructure in cal.com** (`TIER_DISTRIBUTION.md`).
#[test]
fn a_locally_bound_append_is_not_a_forge_write() {
    let decision = decide(
        r#"
        import { useFieldArray, useForm } from "react-hook-form";
        export default function Locations() {
          const { control } = useForm();
          const { fields, append, remove } = useFieldArray({ control, name: "locations" });
          return (
            <div>
              <button onClick={() => append({ type: "link" })}>add</button>
              <button onClick={() => remove(0)}>drop</button>
            </div>
          );
        }
        "#,
    );
    assert_ne!(
        decision.reason,
        TieringReason::ServerOwnedState,
        "a destructured `append` is local form state, not a FORGE write"
    );
    assert_eq!(decision.tier, Tier::C);
}

/// Tier A is untouched by any of this — no state, no ownership question.
#[test]
fn a_static_component_is_unaffected() {
    let decision = decide(
        r#"
        export default function Hero() {
          return <h1>ALBEDO</h1>;
        }
        "#,
    );
    assert_eq!(decision.tier, Tier::A);
    assert_eq!(decision.reason, TieringReason::PureStaticEligible);
}
