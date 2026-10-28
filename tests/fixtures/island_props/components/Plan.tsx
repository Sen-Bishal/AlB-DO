// A plain Tier-A child — no hooks, no handler — so its props reach it through
// the evaluator's component path rather than the island capture.
export default function Plan({ only, onboarding }) {
  return (
    <p className="plan">
      plan {only} after {onboarding}
    </p>
  );
}
