use crate::manifest::schema::{HydrationMode, Tier};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum EffectKind {
    Pure,
    Hooks,
    Async,
    Io,
    SideEffects,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct EffectProfile {
    pub hooks: bool,
    pub asynchronous: bool,
    pub io: bool,
    pub side_effects: bool,
}

impl EffectProfile {
    pub fn pure() -> Self {
        Self::default()
    }

    pub fn join(self, other: Self) -> Self {
        Self {
            hooks: self.hooks || other.hooks,
            asynchronous: self.asynchronous || other.asynchronous,
            io: self.io || other.io,
            side_effects: self.side_effects || other.side_effects,
        }
    }

    pub fn is_pure(&self) -> bool {
        !self.hooks && !self.asynchronous && !self.io && !self.side_effects
    }

    pub fn dominant_kind(&self) -> EffectKind {
        if self.side_effects {
            EffectKind::SideEffects
        } else if self.io {
            EffectKind::Io
        } else if self.asynchronous {
            EffectKind::Async
        } else if self.hooks {
            EffectKind::Hooks
        } else {
            EffectKind::Pure
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TieringReason {
    PureStaticEligible,
    /// Item 4.9 T1. The component holds state that is **proven to escape the
    /// client** — shared, persisted, or server-computed — so the server owns it
    /// and the updates ride the wire instead of an island.
    ServerOwnedState,
    HookDrivenHydration,
    AsyncBoundary,
    IoBoundary,
    SideEffectBoundary,
    WeightBasedPromotion,
    /// AUTH § 3. The component reads the request's principal, so its render
    /// cannot be hoisted out of the request — not to build time (Tier A) and not
    /// to boot time (a Tier-C island's props). Tier B is the only tier that has
    /// a request to read.
    RequestScoped,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TieringDecision {
    pub tier: Tier,
    pub hydration_mode: HydrationMode,
    pub reason: TieringReason,
}

#[derive(Debug, Clone, Copy)]
pub struct TieringInputs {
    pub tier_a_inline_max_bytes: u64,
    pub tier_c_split_min_bytes: u64,
    pub tier_b_mode: HydrationMode,
    pub tier_c_mode: HydrationMode,
}

/// Decide a component's render tier.
///
/// Two interactivity signals are distinguished (dataflow tier design, step 2):
/// - `has_event_handler` — the component declares any `on*` handler. It can no longer be a static
///   Tier-A node, and it hydrates on interaction.
/// - `client_interactive` — at least one handler is provably client-satisfiable (its closure
///   reaches no server boundary). This is the lever that promotes to Tier-C (client island, zero
///   round-trip). A handler that must round-trip (e.g. `onClick` → `fetch`) leaves this false,
///   keeping the component Tier-B.
///   ⚠️ **Measured 2026-08-05: this bit is true for 534 of 536 interactive components in the
///   real-world corpus** (`TIER_DISTRIBUTION.md`). Its negative case is a 12-name list that real
///   code almost never hits, so on its own it is closer to a constant than a lever. That is why
///   `state_escapes` exists and why it is consulted first.
/// - `state_escapes` — item 4.9 T1 · **who owns the state.** True when the state is *proven* to
///   leave this client (a `useSharedSlot` topic, a FORGE write, a server `action`, a network
///   boundary), which makes it the server's and keeps the component Tier-B with no island.
///   **False means "not proven to escape", never "proven local"** — the unknown case must round
///   toward Tier C, because a wrong Tier C still works (an island is the general fallback) while a
///   wrong Tier B ships a binding for state the wire cannot drive.
/// - `reads_principal` — AUTH § 3 · **the render needs the request.** True when the component
///   names `user`, read off the AST by `parser::scan_reads_principal`. It is not a tier *preference*
///   but a tier *impossibility*: Tier-A markup is baked once at build time, so there is no request
///   for a principal to come from, and a component baked that way renders its anonymous branch
///   forever. Like `state_escapes` this is a proof rather than a guess — the distinction
///   `TODO.md` P-c asks this signature to stop losing.
pub fn decide_tier_and_hydration(
    effects: EffectProfile,
    has_event_handler: bool,
    client_interactive: bool,
    state_escapes: bool,
    reads_principal: bool,
    is_above_fold: bool,
    weight_bytes: u64,
    inputs: TieringInputs,
) -> TieringDecision {
    if effects.side_effects {
        // A `useEffect`/`useLayoutEffect` component must run its effect on mount
        // — it wires listeners, subscriptions, or DOM mutations that the user
        // never explicitly triggers (e.g. a scroll-progress bar). So it must
        // hydrate *eagerly*, never `OnInteraction`: a passive effect island
        // would otherwise sit dead because no interaction ever lands on it.
        // Above-fold → `Immediate` (hydrate ASAP); below-fold → `OnIdle`
        // (hydrate at the first idle window). Both resolve to the client's Idle
        // trigger today; the distinction is kept for future trigger granularity.
        return TieringDecision {
            tier: Tier::C,
            hydration_mode: if is_above_fold {
                HydrationMode::Immediate
            } else {
                HydrationMode::OnIdle
            },
            reason: TieringReason::SideEffectBoundary,
        };
    }

    if effects.io {
        return TieringDecision {
            tier: Tier::C,
            hydration_mode: if has_event_handler {
                HydrationMode::OnInteraction
            } else {
                inputs.tier_c_mode
            },
            reason: TieringReason::IoBoundary,
        };
    }

    if effects.asynchronous {
        let promote_to_tier_c = client_interactive || weight_bytes >= inputs.tier_c_split_min_bytes;
        return if promote_to_tier_c {
            TieringDecision {
                tier: Tier::C,
                hydration_mode: if has_event_handler {
                    HydrationMode::OnInteraction
                } else {
                    inputs.tier_c_mode
                },
                reason: TieringReason::AsyncBoundary,
            }
        } else {
            TieringDecision {
                tier: Tier::B,
                // RSC: an async component with no client interaction entry point
                // (no event handler) is a *server data* component — render+await
                // on the server and ship static HTML. It must NOT hydrate: a
                // client island would re-invoke the component in the browser,
                // get a Promise, and clobber the server-injected markup with an
                // empty render. Only async components that also carry a
                // round-tripping handler keep a hydration trigger.
                hydration_mode: if has_event_handler {
                    inputs.tier_b_mode
                } else {
                    HydrationMode::None
                },
                reason: TieringReason::AsyncBoundary,
            }
        };
    }

    if effects.hooks {
        // Item 4.9 T1 · **state ownership decides B vs C.**
        //
        // If the state is proven to leave this client — a `useSharedSlot`
        // topic, a FORGE write, a server `action` — then the *server* owns it.
        // Its updates already have to travel as opcodes so every other client
        // sees them, and the component ships no code.
        //
        // 🔑 This is checked BEFORE `client_interactive` on purpose. A shared
        // counter with an `onClick` is client-satisfiable *and* server-owned;
        // the old cascade saw only the first fact and shipped an island for
        // state the wire was already carrying.
        //
        // ⚠️ Deliberately does NOT override `side_effects`/`io` above: a
        // `useEffect` body must run in the browser no matter who owns the
        // state, so escaping state cannot pull it back to B.
        if state_escapes {
            return TieringDecision {
                tier: Tier::B,
                hydration_mode: inputs.tier_b_mode,
                reason: TieringReason::ServerOwnedState,
            };
        }

        return if client_interactive {
            TieringDecision {
                tier: Tier::C,
                hydration_mode: HydrationMode::OnInteraction,
                reason: TieringReason::HookDrivenHydration,
            }
        } else {
            TieringDecision {
                tier: Tier::B,
                hydration_mode: inputs.tier_b_mode,
                reason: TieringReason::HookDrivenHydration,
            }
        };
    }

    // A handler with no hooks/effects still must hydrate to run — never Tier-A.
    //
    // AUTH § 3 · and neither may a component that reads the principal. Tier-A
    // markup is baked once, at build time, into the manifest's `tier_a_root`;
    // there is no request in scope, so `user` is whatever it was when the build
    // ran — nothing. A component baked that way does not fail, which is the
    // problem: it renders its anonymous branch to everyone, forever, and the
    // signed-in branch is absent from the artifact rather than merely unreached.
    // Falling through leaves it Tier B, where `RequestContext::resolve("user")`
    // runs per request.
    if weight_bytes <= inputs.tier_a_inline_max_bytes && !has_event_handler && !reads_principal {
        return TieringDecision {
            tier: Tier::A,
            hydration_mode: HydrationMode::None,
            reason: TieringReason::PureStaticEligible,
        };
    }

    // 🔑 Checked before the weight rule below, which would otherwise promote a
    // large principal-reading component to Tier C — an island whose props are
    // computed once at boot, so it has the same no-request problem Tier A does,
    // one tier up. A component that needs the request is served from the tier
    // that has one.
    if reads_principal {
        return TieringDecision {
            tier: Tier::B,
            hydration_mode: inputs.tier_b_mode,
            reason: TieringReason::RequestScoped,
        };
    }

    if weight_bytes >= inputs.tier_c_split_min_bytes
        || (client_interactive && weight_bytes > inputs.tier_a_inline_max_bytes)
    {
        return TieringDecision {
            tier: Tier::C,
            hydration_mode: if has_event_handler {
                HydrationMode::OnInteraction
            } else {
                inputs.tier_c_mode
            },
            reason: TieringReason::WeightBasedPromotion,
        };
    }

    TieringDecision {
        tier: Tier::B,
        hydration_mode: inputs.tier_b_mode,
        reason: TieringReason::WeightBasedPromotion,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> TieringInputs {
        TieringInputs {
            tier_a_inline_max_bytes: 8 * 1024,
            tier_c_split_min_bytes: 40 * 1024,
            tier_b_mode: HydrationMode::OnIdle,
            tier_c_mode: HydrationMode::OnVisible,
        }
    }

    #[test]
    fn test_pure_small_component_is_tier_a() {
        let decision =
            decide_tier_and_hydration(EffectProfile::pure(), false, false, false, false, false, 1024, inputs());
        assert_eq!(decision.tier, Tier::A);
        assert_eq!(decision.hydration_mode, HydrationMode::None);
        assert_eq!(decision.reason, TieringReason::PureStaticEligible);
    }

    #[test]
    fn test_handler_without_hooks_is_not_tier_a() {
        // A pure component that nonetheless declares an `on*` handler must
        // hydrate to run the handler — it can never collapse to static Tier-A.
        let decision =
            decide_tier_and_hydration(EffectProfile::pure(), true, false, false, false, false, 1024, inputs());
        assert_ne!(decision.tier, Tier::A);
    }

    #[test]
    fn test_hook_component_is_not_tier_a() {
        let decision = decide_tier_and_hydration(
            EffectProfile {
                hooks: true,
                ..EffectProfile::default()
            },
            false,
            false,
            false,
            false,
            false,
            1024,
            inputs(),
        );
        assert_eq!(decision.tier, Tier::B);
        assert_eq!(decision.reason, TieringReason::HookDrivenHydration);
    }

    fn tier_of(source: &str, file: &str) -> TieringDecision {
        use crate::parser::ComponentParser;
        let parsed = ComponentParser::new().parse_source(source, file).unwrap();
        let c = &parsed[0];
        decide_tier_and_hydration(
            c.effect_profile,
            c.is_interactive,
            c.is_client_interactive,
            c.state_escapes,
            c.reads_principal,
            false,
            1024,
            inputs(),
        )
    }

    /// AUTH § 3 · the defect this signal exists for.
    ///
    /// Before `reads_principal`, this component was small and pure, so it took
    /// the Tier-A branch and its markup was baked into the manifest at build
    /// time — with `user` undefined, because there is no request at build time.
    /// It did not fail; it rendered the anonymous branch to everyone forever,
    /// and the signed-in branch was absent from the artifact entirely.
    #[test]
    fn a_component_that_reads_user_is_never_tier_a() {
        let decision = tier_of(
            r#"
            export default function Greeting({ user }) {
                return <p>{user ? user.id : "stranger"}</p>;
            }
            "#,
            "Greeting.tsx",
        );
        assert_eq!(decision.tier, Tier::B, "it must be rendered per request");
        assert_eq!(decision.reason, TieringReason::RequestScoped);
    }

    /// Asking for it in the signature is asking for it. A component that
    /// destructures `user` and only passes it down still needs the prop.
    #[test]
    fn destructuring_user_is_enough_even_if_the_body_never_names_it() {
        let decision = tier_of(
            r#"
            export default function Shell({ user }) {
                return <div className="shell" />;
            }
            "#,
            "Shell.tsx",
        );
        assert_eq!(decision.tier, Tier::B);
        assert_eq!(decision.reason, TieringReason::RequestScoped);
    }

    /// 🔑 The size rule must not reclaim it. A large principal-reading component
    /// would otherwise be promoted to Tier C — an island whose props are computed
    /// once at boot, which has the same no-request problem one tier up.
    #[test]
    fn a_large_component_that_reads_user_is_tier_b_not_an_island() {
        let parsed = crate::parser::ComponentParser::new()
            .parse_source(
                r#"
                export default function Big({ user }) {
                    return <p>{user.id}</p>;
                }
                "#,
                "Big.tsx",
            )
            .unwrap();
        let c = &parsed[0];
        let decision = decide_tier_and_hydration(
            c.effect_profile,
            c.is_interactive,
            c.is_client_interactive,
            c.state_escapes,
            c.reads_principal,
            false,
            // Well past `tier_c_split_min_bytes`.
            256 * 1024,
            inputs(),
        );
        assert_eq!(decision.tier, Tier::B);
        assert_eq!(decision.reason, TieringReason::RequestScoped);
    }

    /// The three near-misses, which must all stay Tier A. Each one is a `user`
    /// that is not *the* user, and treating any of them as the principal would
    /// drag an ordinary static component into a per-request render.
    #[test]
    fn a_user_that_is_not_the_principal_does_not_move_the_tier() {
        // A local binding shadows the ambient one — same rule that stops
        // `const { append } = …` from reading as a FORGE write.
        let shadowed = tier_of(
            r#"
            export default function Row() {
                const user = { id: "local" };
                return <p>{user.id}</p>;
            }
            "#,
            "Row.tsx",
        );
        assert_eq!(shadowed.tier, Tier::A, "a local `user` is not the principal");

        // A member-property name: `row.user` is someone else's column.
        let member = tier_of(
            r#"
            export default function Row({ row }) {
                return <p>{row.user}</p>;
            }
            "#,
            "Member.tsx",
        );
        assert_eq!(member.tier, Tier::A, "`row.user` is a field, not the principal");

        // A JSX attribute name: `<Profile user={author} />` names a prop on
        // someone else's component.
        let attr = tier_of(
            r#"
            export default function Page({ author }) {
                return <Profile user={author} />;
            }
            "#,
            "Attr.tsx",
        );
        assert_eq!(attr.tier, Tier::A, "a JSX attr named `user` is not a read");
    }

    #[test]
    fn handler_driven_counter_promotes_to_tier_c_without_name_heuristic() {
        // A component NAMED "Counter" — which the old name heuristic would never
        // flag — reaches Tier-C purely because its `onClick` is client-satisfiable.
        let decision = tier_of(
            r#"
            export default function Counter() {
                const [n, setN] = useState(0);
                return <button onClick={() => setN(n + 1)}>{n}</button>;
            }
            "#,
            "Counter.tsx",
        );
        assert_eq!(decision.tier, Tier::C);
        assert_eq!(decision.reason, TieringReason::HookDrivenHydration);
    }

    #[test]
    fn server_touching_handler_stays_tier_b() {
        // Step 2 discriminator: same shape as Counter, but the handler awaits
        // `fetch` — a server boundary — so it must NOT be a zero-round-trip
        // Tier-C island; it stays Tier-B.
        let decision = tier_of(
            r#"
            export default function LikeButton() {
                const [liked, setLiked] = useState(false);
                return <button onClick={async () => { await fetch('/api/like'); setLiked(true); }}>like</button>;
            }
            "#,
            "LikeButton.tsx",
        );
        assert_eq!(decision.tier, Tier::B);
    }

    #[test]
    fn extracted_local_handler_resolves_to_client_satisfiable() {
        // The handler is a bare identifier resolving to a local pure closure —
        // free-variable resolution must still land it on Tier-C.
        let decision = tier_of(
            r#"
            export default function Stepper() {
                const [n, setN] = useState(0);
                const inc = () => setN(n + 1);
                return <button onClick={inc}>{n}</button>;
            }
            "#,
            "Stepper.tsx",
        );
        assert_eq!(decision.tier, Tier::C);
    }

    #[test]
    fn test_io_component_promotes_to_tier_c() {
        let decision = decide_tier_and_hydration(
            EffectProfile {
                io: true,
                ..EffectProfile::default()
            },
            false,
            false,
            false,
            false,
            false,
            1024,
            inputs(),
        );
        assert_eq!(decision.tier, Tier::C);
        assert_eq!(decision.reason, TieringReason::IoBoundary);
    }

    #[test]
    fn side_effect_component_hydrates_eagerly_not_on_interaction() {
        // A `useEffect`-bearing island (e.g. a scroll-progress bar) must run its
        // effect on mount — it can never wait for an interaction that may never
        // come. Below-fold → OnIdle, above-fold → Immediate; neither is
        // OnInteraction.
        let below = decide_tier_and_hydration(
            EffectProfile {
                side_effects: true,
                ..EffectProfile::default()
            },
            false,
            false,
            false,
            false,
            false,
            1024,
            inputs(),
        );
        assert_eq!(below.tier, Tier::C);
        assert_eq!(below.reason, TieringReason::SideEffectBoundary);
        assert_eq!(below.hydration_mode, HydrationMode::OnIdle);
        assert_ne!(below.hydration_mode, HydrationMode::OnInteraction);

        let above = decide_tier_and_hydration(
            EffectProfile {
                side_effects: true,
                ..EffectProfile::default()
            },
            false,
            false,
            false,
            false,
            true,
            1024,
            inputs(),
        );
        assert_eq!(above.hydration_mode, HydrationMode::Immediate);
    }
}
