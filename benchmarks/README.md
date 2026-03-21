# Benchmark Harness

This directory defines the Phase 0 benchmark contract for Albedo.

## Files

- `workloads.json`: versioned workload definitions, baseline competitor metadata, and p95 gate envelopes.
- `baseline.json`: baseline p95 values used by CI regression checks.

## Design Goals

- Reproducible benchmark scenarios in source control.
- Explicit p95 envelopes for scan/optimize/total pipeline stages.
- CI gate support with configurable regression tolerance.
- Competitor baseline metadata tracked alongside workloads.

## Run

```bash
cargo run --bin albedo-bench -- --config benchmarks/workloads.json --baseline benchmarks/baseline.json --assert-gates --output target/benchmarks/latest.json
```

## Notes

- Keep workload paths deterministic and repository-local.
- Tighten baseline envelopes only after collecting stable CI samples.
- Update `baseline.json` in the same change where benchmark methodology or workload shape changes.
