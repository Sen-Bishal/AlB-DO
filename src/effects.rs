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

pub fn decide_tier_and_hydration(
    effects: EffectProfile,
    is_interactive: bool,
    is_above_fold: bool,
    weight_bytes: u64,
    inputs: TieringInputs,
) -> TieringDecision {
    if effects.side_effects {
        return TieringDecision {
            tier: Tier::C,
            hydration_mode: if is_above_fold {
                HydrationMode::Immediate
            } else {
                HydrationMode::OnInteraction
            },
            reason: TieringReason::SideEffectBoundary,
        };
    }

    if effects.io {
        return TieringDecision {
            tier: Tier::C,
            hydration_mode: if is_interactive {
                HydrationMode::OnInteraction
            } else {
                inputs.tier_c_mode
            },
            reason: TieringReason::IoBoundary,
        };
    }

    if effects.asynchronous {
        let promote_to_tier_c = is_interactive || weight_bytes >= inputs.tier_c_split_min_bytes;
        return if promote_to_tier_c {
            TieringDecision {
                tier: Tier::C,
                hydration_mode: if is_interactive {
                    HydrationMode::OnInteraction
                } else {
                    inputs.tier_c_mode
                },
                reason: TieringReason::AsyncBoundary,
            }
        } else {
            TieringDecision {
                tier: Tier::B,
                hydration_mode: inputs.tier_b_mode,
                reason: TieringReason::AsyncBoundary,
            }
        };
    }

    if effects.hooks {
        return if is_interactive {
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

    if weight_bytes <= inputs.tier_a_inline_max_bytes && !is_interactive {
        return TieringDecision {
            tier: Tier::A,
            hydration_mode: HydrationMode::None,
            reason: TieringReason::PureStaticEligible,
        };
    }

    if weight_bytes >= inputs.tier_c_split_min_bytes
        || (is_interactive && weight_bytes > inputs.tier_a_inline_max_bytes)
    {
        return TieringDecision {
            tier: Tier::C,
            hydration_mode: if is_interactive {
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
            decide_tier_and_hydration(EffectProfile::pure(), false, false, 1024, inputs());
        assert_eq!(decision.tier, Tier::A);
        assert_eq!(decision.hydration_mode, HydrationMode::None);
        assert_eq!(decision.reason, TieringReason::PureStaticEligible);
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
            1024,
            inputs(),
        );
        assert_eq!(decision.tier, Tier::B);
        assert_eq!(decision.reason, TieringReason::HookDrivenHydration);
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
            1024,
            inputs(),
        );
        assert_eq!(decision.tier, Tier::C);
        assert_eq!(decision.reason, TieringReason::IoBoundary);
    }
}
