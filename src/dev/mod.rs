pub mod benchmark;
pub mod contract;
pub mod proc_bench;
pub mod serve_bench;
pub mod showcase;

pub use benchmark::{
    run_workloads, write_report_json, BaselineCompetitor, BaselineEnvelopeFile,
    BaselineScenarioEnvelope, BenchmarkError, BenchmarkReport, BenchmarkScenario,
    BenchmarkWorkloads, GateStatus, MetricSummary, RegressionPolicy, ScenarioBenchmarkResult,
    ScenarioGateReport, ScenarioMetrics,
};
pub use serve_bench::{
    measure_single, probe_tcp_ready, run as run_serve_bench, ColdSample, EndpointResult,
    LatencyStats, Method, RequestSpec, ServeBenchConfig, ServeBenchError, ServeBenchReport,
};
pub use proc_bench::{
    run_cold_starts, ColdStartConfig, ColdStartReport, ColdStartSample, ProcBenchError,
    ProcessSpawner, ServerProcess, Spawner,
};
pub use proc_bench::build_bench::{
    run_build_bench, BuildBenchConfig, BuildBenchError, BuildBenchReport, BuildWorkload,
    CommandBuildWorkload,
};
pub use contract::{
    parse_dev_cli_args, resolve_dev_contract, DevCliOptions, DevConfig, DevHmrConfig,
    DevServerConfig, DevWatchConfig, HmrTransport, HotSetPriority, HotSetRegistration,
    ResolvedDevContract, StaticSliceConfig, DEV_CONFIG_JSON, DEV_CONFIG_TS,
};
pub use showcase::{
    build_showcase_artifact, render_showcase_document, ShowcaseArtifact, ShowcaseDependencyHash,
    ShowcaseGraphStats, ShowcaseRenderRequest, ShowcaseStats, ShowcaseTimings,
};
