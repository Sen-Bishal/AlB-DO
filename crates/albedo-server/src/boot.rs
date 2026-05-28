//! Phase P · Stream A — production server boot.
//!
//! [`boot_production_server`] is the integration seam that converts
//! a built project (manifest + emitted assets under `.albedo/dist/`)
//! and its source tree into a fully wired [`AlbedoServer`]. The
//! existing [`AlbedoServerBuilder`] API stays untouched; this module
//! exists so the `albedo serve` CLI has one call to make instead of
//! reimplementing the wiring inline.
//!
//! Audit gaps closed:
//!
//! * **#1** — `albedo serve` becomes a real `AlbedoServer` boot
//!   instead of a static file server.
//! * **#5** — [`AlbedoServerBuilder::register_compiled_project`]
//!   gets its first production caller.
//! * **#6** — [`dom_render_compiler::runtime::CompiledProject::load_from_dir`]
//!   gets its first production caller.
//! * **#8** — every Phase K `onClick` handler that Stream B baked
//!   into the manifest's `initial_opcode_frame` now resolves to a
//!   live `ActionHandler` via the registered `CompiledProject`.

use crate::config::{AppConfig, RouteSpec, ServerConfig};
use crate::error::RuntimeError;
use crate::renderer_runtime::RendererRuntime;
use crate::routing::HttpMethod;
use crate::server::{AlbedoServer, AlbedoServerBuilder};
use dom_render_compiler::dev_contract::ResolvedDevContract;
use dom_render_compiler::runtime::CompiledProject;
use std::path::PathBuf;
use std::sync::Arc;

/// Inputs to [`boot_production_server`]. Held as a separate struct so
/// integration tests can construct fixtures without going through
/// [`ResolvedDevContract`].
#[derive(Debug, Clone)]
pub struct ProductionServerOptions {
    /// Project root — contains `public/`, `.albedo/`, `package.json`.
    pub project_dir: PathBuf,
    /// Source root — typically `<project_dir>/src`; the JSX/TSX
    /// tree [`CompiledProject::load_from_dir`] walks.
    pub source_root: PathBuf,
    /// Build output — typically `<project_dir>/.albedo/dist`. Must
    /// contain `render-manifest.v2.json` and the bakabox runtime
    /// assets under `_albedo/`.
    pub dist_dir: PathBuf,
    /// Bind host. Same default as `albedo dev` (`127.0.0.1`).
    pub host: String,
    /// Bind port. Same default as `albedo dev` (3000).
    pub port: u16,
}

impl ProductionServerOptions {
    /// Derive the standard option shape from a resolved dev contract.
    /// Mirrors the layout the build pipeline emits — `.albedo/dist/`
    /// under the project root, source files under `contract.root`.
    pub fn from_contract(contract: &ResolvedDevContract) -> Self {
        let dist_dir = contract.project_dir.join(".albedo").join("dist");
        Self {
            project_dir: contract.project_dir.clone(),
            source_root: contract.root.clone(),
            dist_dir,
            host: contract.server.host.clone(),
            port: contract.server.port,
        }
    }
}

/// Construct an [`AlbedoServer`] from a built project on disk.
///
/// The caller is responsible for running `albedo build` first — this
/// function assumes `dist_dir/render-manifest.v2.json` exists.
/// Failures surface as [`RuntimeError`] so the CLI can print a clean
/// "did you run `albedo build`?" hint.
///
/// Steps:
///
/// 1. Load the build-time [`RendererRuntime`] from `dist_dir`. The
///    manifest carries pre-rendered Tier-B HTML + opcodes from
///    Stream B, so the streaming handler ships them verbatim without
///    re-rendering.
/// 2. Synthesise one `RouteSpec` per manifest route so the manifest-
///    streaming arm of [`AlbedoServer::dispatch`] activates. The
///    `handler` id is a non-resolving placeholder — the streaming
///    path bypasses [`AppConfig.routes`]'s `handler` lookup when
///    `should_use_manifest_streaming` returns true.
/// 3. Load every JSX/TSX module via [`CompiledProject::load_from_dir`]
///    and register it through
///    [`AlbedoServerBuilder::register_compiled_project`]. Every
///    Phase K `onClick` plus every Stream C TS `action()` declaration
///    becomes a live `ActionHandler` keyed by its FNV-1a-32 id.
/// 4. Mount `public/` directories: user-authored first, then the
///    build-time mirror under `dist/public`, then the dist root
///    itself so the bakabox runtime files at `dist/_albedo/*.js`
///    resolve at `/_albedo/*`.
pub fn boot_production_server(
    opts: &ProductionServerOptions,
) -> Result<AlbedoServer, RuntimeError> {
    if !opts.dist_dir.is_dir() {
        return Err(RuntimeError::ServerStartup(format!(
            "build output directory '{}' is missing — run `albedo build` first",
            opts.dist_dir.display()
        )));
    }

    let renderer = RendererRuntime::from_artifacts_dir(opts.dist_dir.clone())?;

    // Every URL in the manifest becomes a GET route whose dispatch
    // hits the manifest-streaming arm. `entry_module: Some(_)` is
    // load-bearing — `should_use_manifest_streaming` rejects routes
    // with `entry_module.is_none()`. The handler id never resolves,
    // and that's fine: the streaming arm runs before the handler
    // lookup.
    let routes: Vec<RouteSpec> = renderer
        .manifest()
        .routes
        .keys()
        .map(|path| RouteSpec {
            name: format!("albedo:manifest:{path}"),
            method: HttpMethod::Get,
            path: path.clone(),
            handler: "albedo-manifest-streaming".to_string(),
            entry_module: Some(path.clone()),
            props_loader: None,
            middleware: Vec::new(),
            auth: None,
        })
        .collect();

    let compiled = CompiledProject::load_from_dir(&opts.source_root).map_err(|err| {
        RuntimeError::ServerStartup(format!(
            "failed to load source tree at '{}': {err}",
            opts.source_root.display()
        ))
    })?;

    let app_config = AppConfig {
        server: ServerConfig {
            host: opts.host.clone(),
            port: opts.port,
            ..ServerConfig::default()
        },
        renderer: None,
        layouts: Vec::new(),
        routes,
    };

    // Force dev mode off so a release `albedo serve` never leaks the
    // overlay/HMR endpoints. The inspector follows the same default
    // policy; tests can flip it back on via the builder.
    let mut builder = AlbedoServerBuilder::new(app_config)
        .with_renderer_runtime(renderer)
        .with_dev_mode(false)
        .register_compiled_project(Arc::new(compiled));

    let user_public = opts.project_dir.join("public");
    if user_public.is_dir() {
        builder = builder.with_public_dir(user_public);
    }
    let dist_public = opts.dist_dir.join("public");
    if dist_public.is_dir() {
        builder = builder.with_public_dir(dist_public);
    }
    // Phase P · post-P wire-through — `<dist>` is intentionally NOT
    // mounted as a public_dir. The previous design served bakabox
    // runtime assets from `<dist>/_albedo/*` via the public mount,
    // but that also exposed `<dist>/index.html` at `/`, which
    // shadowed the manifest-streaming arm for the root route. The
    // runtime assets now route through `dispatch_albedo_asset` in
    // `AlbedoServer::dispatch`, served from the binary's embedded
    // templates — same source as the dev path's `dev_static_asset`.

    builder.build()
}
