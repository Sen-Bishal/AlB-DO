pub mod benchmark;
pub mod contract;
pub mod showcase;

pub use benchmark::{
    run_workloads, write_report_json, BaselineCompetitor, BaselineEnvelopeFile,
    BaselineScenarioEnvelope, BenchmarkError, BenchmarkReport, BenchmarkScenario,
    BenchmarkWorkloads, GateStatus, MetricSummary, RegressionPolicy, ScenarioBenchmarkResult,
    ScenarioGateReport, ScenarioMetrics,
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
