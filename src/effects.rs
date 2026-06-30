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
    HookDrivenHydration,
    AsyncBoundary,
    IoBoundary,
    SideEffectBoundary,
    WeightBasedPromotion,
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
pub fn decide_tier_and_hydration(
    effects: EffectProfile,
    has_event_handler: bool,
    client_interactive: bool,
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
    if weight_bytes <= inputs.tier_a_inline_max_bytes && !has_event_handler {
        return TieringDecision {
            tier: Tier::A,
            hydration_mode: HydrationMode::None,
            reason: TieringReason::PureStaticEligible,
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
            decide_tier_and_hydration(EffectProfile::pure(), false, false, false, 1024, inputs());
        assert_eq!(decision.tier, Tier::A);
        assert_eq!(decision.hydration_mode, HydrationMode::None);
        assert_eq!(decision.reason, TieringReason::PureStaticEligible);
    }

    #[test]
    fn test_handler_without_hooks_is_not_tier_a() {
        // A pure component that nonetheless declares an `on*` handler must
        // hydrate to run the handler — it can never collapse to static Tier-A.
        let decision =
            decide_tier_and_hydration(EffectProfile::pure(), true, false, false, 1024, inputs());
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
            false,
            1024,
            inputs(),
        )
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
            true,
            1024,
            inputs(),
        );
        assert_eq!(above.hydration_mode, HydrationMode::Immediate);
    }
}
