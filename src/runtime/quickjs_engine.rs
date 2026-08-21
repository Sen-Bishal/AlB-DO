use super::arena::{ArenaAllocator, ArenaControl, ArenaStats};
use super::bridge::{
    build_handler_script, decode_handler_run, HandlerInvocation, HandlerOutcome, HandlerRun,
};
use super::engine::{
    stable_source_hash, BootstrapPayload, LoadErrorKind, RenderOutput, RuntimeEngine, RuntimeError,
    RuntimeResult,
};
use rquickjs::{promise::MaybePromise, Context, Ctx, Function, Runtime};
use serde::Deserialize;
use crate::runtime::eval::component::fnv1a_32;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use swc_common::{
    comments::SingleThreadedComments, sync::Lrc, FileName, Globals, Mark, SourceMap, Span, Spanned,
    GLOBALS,
};
use swc_ecma_ast::{
    Decl, ExportSpecifier, ImportDecl, ImportSpecifier, Module, ModuleDecl, ModuleExportName,
    ModuleItem, Pat,
};
use swc_ecma_codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_base::resolver;

const MAX_MODULE_SIZE: usize = 10 * 1024 * 1024; // 10 MB limit
use swc_ecma_transforms_react::{jsx, Options as JsxOptions, Runtime as JsxRuntime};
use swc_ecma_transforms_typescript::strip_type;
use swc_ecma_visit::VisitMutWith;

const MODULE_RECORD_FLAG: &str = "__albedo_is_module_record";
const MODULE_MISSING_MARKER: &str = "__ALBEDO_MODULE_MISSING__:";
const INVALID_ENTRY_EXPORT_MARKER: &str = "__ALBEDO_INVALID_ENTRY_EXPORT__:";

#[derive(Debug, Deserialize)]
struct RenderEnvelope {
    ok: bool,
    value: Option<String>,
    error: Option<String>,
}

/// Result envelope for [`QuickJsEngine::eval_route_metadata`]. Unlike
/// [`RenderEnvelope`], `value` carries the raw `generateMetadata` object (the
/// Next.js `Metadata` shape) rather than a rendered HTML string.
#[derive(Debug, Deserialize)]
struct MetadataEnvelope {
    ok: bool,
    value: Option<serde_json::Value>,
    error: Option<String>,
}

/// Number of leading renders that run in persistent (non-reset) mode so QuickJS can
/// allocate its lazily-created, data-dependent runtime-global infrastructure (shape and
/// atom tables) into the persistent region before request-scoped reset is enabled.
///
/// This counter-based window is the *implicit* warm-up used by the single-threaded boot
/// renderer (it renders every route during the window). Code paths that must warm
/// engines with a known set of components up front — e.g. the multi-engine action/
/// render pool — instead use the *explicit* [`QuickJsEngine::begin_warmup`] /
/// [`QuickJsEngine::end_warmup`] bracket, which forces persistent mode irrespective of
/// the counter so an arbitrary number of warm-up renders all intern into the persistent
/// region. Both mechanisms exist for the same reason: any retained (interned) state a
/// render or handler creates *after* warm-up lands in the request region, which the
/// boundary reset then frees — a use-after-free that corrupts the runtime.
const ARENA_WARMUP_RENDERS: u32 = 8;

pub struct QuickJsEngine {
    runtime: Option<Runtime>,
    context: Option<Context>,
    arena: Arc<ArenaControl>,
    renders_done: u32,
    /// When set, renders/handlers run in persistent (non-reset) mode regardless of
    /// `renders_done`. Toggled by [`Self::begin_warmup`] / [`Self::end_warmup`] so a
    /// caller can warm an engine with a specific component set whose interned state
    /// must survive in the persistent region. See [`ARENA_WARMUP_RENDERS`].
    force_persistent: bool,
    loaded_module_hashes: HashMap<String, u64>,
    bootstrap: Option<BootstrapPayload>,
    initialized: bool,
}

impl QuickJsEngine {
    pub fn new() -> Self {
        Self {
            runtime: None,
            context: None,
            arena: ArenaControl::with_default_caps(),
            renders_done: 0,
            force_persistent: false,
            loaded_module_hashes: HashMap::new(),
            bootstrap: None,
            initialized: false,
        }
    }

    /// Snapshot of the request-scoped bump arena that backs the QuickJS runtime.
    pub fn arena_stats(&self) -> ArenaStats {
        self.arena.stats()
    }

    pub fn prewarm(&mut self) {
        if self.initialized {
            return;
        }
        let _ = self.ensure_initialized();
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Enter explicit warm-up mode: until [`Self::end_warmup`], every render and
    /// handler eval runs in persistent (non-reset) arena mode regardless of how
    /// many operations have already run. Use this to warm an engine with a known
    /// set of components whose interned QuickJS state (shapes/atoms) must live in
    /// the persistent region before the engine starts serving request-scoped work.
    /// See [`ARENA_WARMUP_RENDERS`] for the implicit, counter-based alternative.
    pub fn begin_warmup(&mut self) {
        self.force_persistent = true;
    }

    /// Leave explicit warm-up mode and arm request-scoped reset for all subsequent
    /// work. Advances the warm-up counter past [`ARENA_WARMUP_RENDERS`] so the
    /// implicit window can't reopen and re-admit a persistent render after the
    /// engine has been declared hot.
    pub fn end_warmup(&mut self) {
        self.force_persistent = false;
        self.renders_done = self.renders_done.max(ARENA_WARMUP_RENDERS);
    }

    /// A1 · host-object bridge — run a TSX event handler / server `action()`
    /// body under QuickJS and collect the slot-write and broadcast effects it
    /// produced, in source order.
    ///
    /// Unlike [`RuntimeEngine::render_component`], which lowers a component to
    /// HTML, this evaluates a handler body for its *effects*: `setX(v)` calls
    /// become [`HandlerEffect::SlotSet`], `broadcast(topic, v)` calls become
    /// [`HandlerEffect::Broadcast`]. Each lowers to the same
    /// [`crate::ir::opcode::Instruction::SlotSet`] the action dispatcher already
    /// ships, so the wire shape matches the pure-Rust handler path exactly —
    /// the difference is that the body now runs in a full JS engine, so loops,
    /// `try`/`catch`, array methods, and anything else the pure-Rust
    /// interpreter rejected just work.
    ///
    /// A throw inside the body surfaces as a loud `RenderError` rather than a
    /// silently dropped effect.
    ///
    /// Returns both the body's side-effects and its completion value (a form
    /// action's validation return); see [`HandlerOutcome`]. The result rides the
    /// same copied-out JSON string as the effects — no extra engine round-trip.
    ///
    /// Runs under the same request-scoped arena discipline as a render: after
    /// warmup the body bump-allocates into the request region, the effect JSON
    /// is copied out into Rust, then the boundary reset reclaims the region.
    pub fn eval_handler(
        &mut self,
        entry: &str,
        invocation: &HandlerInvocation,
    ) -> RuntimeResult<HandlerOutcome> {
        match self.eval_handler_run(entry, invocation)? {
            HandlerRun::Completed(outcome) => Ok(outcome),
            HandlerRun::Suspended { pending, .. } => Err(RuntimeError::render(format!(
                "handler '{entry}' called fetch() ({} request(s) staged) on a path that cannot \
                 replay it; use `eval_handler_run` and drive the passes",
                pending.len()
            ))),
        }
    }

    /// APERTURE A2 · one pass of a handler body — completion **or** a request
    /// for I/O.
    ///
    /// [`Self::eval_handler`] is this with the second case declared impossible,
    /// which is right for every caller that has no journal to replay against.
    pub fn eval_handler_run(
        &mut self,
        entry: &str,
        invocation: &HandlerInvocation,
    ) -> RuntimeResult<HandlerRun> {
        self.ensure_initialized()?;
        let script = build_handler_script(invocation)?;

        let scoped = !self.force_persistent && self.renders_done >= ARENA_WARMUP_RENDERS;
        self.renders_done = self.renders_done.saturating_add(1);
        if scoped {
            self.arena.begin_request();
        }
        let eval_result = self.context.as_ref().unwrap().with(|ctx| {
            ctx.eval::<String, _>(script.as_str()).map_err(|err| {
                RuntimeError::render(format!("failed to execute handler '{entry}': {err}"))
            })
        });
        if scoped {
            self.runtime.as_ref().expect("runtime initialized").run_gc();
            self.arena.end_request();
        }

        let envelope_json = eval_result?;
        decode_handler_run(entry, &envelope_json)
    }

    /// Shared body for [`RuntimeEngine::render_component`] and
    /// [`RuntimeEngine::render_component_with_host`]. `host_json = None` renders
    /// with no seed (every hook uses its initial) — byte-identical to the
    /// pre-bridge behaviour, so the host-unaware path is untouched.
    ///
    /// When a `host` envelope is present it is installed as
    /// `globalThis.__ALBEDO_HOST` for the duration of the render: `useState`
    /// pairs its positional call with `host.state[idx]` (falling back to the
    /// call's initial when the seed omits that index), and
    /// `useSharedSlot("topic")` reads `host.shared[topic]`. This is what lets a
    /// server render reflect the *current* slot-store / broadcast values rather
    /// than always re-rendering from each hook's initial. A malformed seed
    /// surfaces as a loud render error rather than silently rendering initials.
    fn render_component_inner(
        &mut self,
        entry: &str,
        props_json: &str,
        host_json: Option<&str>,
    ) -> RuntimeResult<RenderOutput> {
        self.ensure_initialized()?;

        // Movement III: after a short warmup (during which QuickJS finishes allocating its
        // retained, data-dependent runtime-global tables into the persistent region),
        // everything a render allocates is bump-allocated into the request region. The
        // result string is copied out into Rust below, then a single cycle-collection pass
        // drops any cyclic request garbage so the O(1) reset can reclaim the region with
        // nothing left referencing it.
        let scoped = !self.force_persistent && self.renders_done >= ARENA_WARMUP_RENDERS;
        self.renders_done = self.renders_done.saturating_add(1);
        if scoped {
            self.arena.begin_request();
        }
        let host_arg = host_json.unwrap_or("").to_string();
        let eval_start = Instant::now();
        let render_result = self.context.as_ref().unwrap().with(|ctx| {
            let globals = ctx.globals();
            let render_fn: Function = globals.get("__ALBEDO_RENDER_COMPONENT").map_err(|err| {
                RuntimeError::render(format!(
                    "reusable render function missing for component '{entry}': {err}"
                ))
            })?;

            let maybe = render_fn
                .call::<(String, String, String), MaybePromise>((
                    entry.to_string(),
                    props_json.to_string(),
                    host_arg,
                ))
                .map_err(|err| {
                    RuntimeError::render(format!(
                        "failed to execute reusable render function for component '{entry}': {err}"
                    ))
                })?;
            // An async server component (RSC) returns a Promise of its envelope;
            // `finish` drives the QuickJS job queue to resolution here, on the
            // server. A synchronous component is already settled, so this is the
            // no-op fast path. If resolution can't progress — the awaited work
            // needs host I/O the SSR sandbox can't provide — `finish` yields
            // `WouldBlock`, surfaced as a loud render error rather than a blank.
            maybe.finish::<String>().map_err(|err| {
                RuntimeError::render(format!(
                    "failed to resolve render result for component '{entry}': {err}"
                ))
            })
        });
        let eval_ms = eval_start.elapsed().as_millis();

        if scoped {
            self.runtime.as_ref().expect("runtime initialized").run_gc();
            self.arena.end_request();
        }

        let envelope_json = render_result?;
        let envelope: RenderEnvelope = serde_json::from_str(&envelope_json).map_err(|err| {
            RuntimeError::render(format!(
                "failed to decode render result envelope for '{entry}': {err}"
            ))
        })?;

        if envelope.ok {
            let html = envelope.value.ok_or_else(|| {
                RuntimeError::render(format!(
                    "render script for '{entry}' returned success without value"
                ))
            })?;
            Ok(RenderOutput { html, eval_ms })
        } else {
            let message = envelope
                .error
                .unwrap_or_else(|| "unknown runtime error".to_string());
            Err(map_render_error(entry, &message))
        }
    }

    /// Gate 2 · B slice 3 — evaluate a route module's `generateMetadata(props)`
    /// export to its raw metadata object. `Ok(None)` when the module declares no
    /// such export (the common case — the static `<head>` stands); `Ok(Some)`
    /// with the resolved object otherwise. An `async generateMetadata` is driven
    /// to settlement here, the same way an async server component is awaited
    /// during render. A throw inside `generateMetadata` surfaces as a loud
    /// render error rather than a silent empty head.
    pub fn eval_route_metadata(
        &mut self,
        entry: &str,
        props_json: &str,
    ) -> RuntimeResult<Option<serde_json::Value>> {
        self.ensure_initialized()?;

        // Same request-arena discipline as a render: after warmup, everything
        // the eval allocates is bump-allocated into the request region and reset
        // in O(1) once the result string is copied out.
        let scoped = !self.force_persistent && self.renders_done >= ARENA_WARMUP_RENDERS;
        self.renders_done = self.renders_done.saturating_add(1);
        if scoped {
            self.arena.begin_request();
        }

        let eval_result = self.context.as_ref().unwrap().with(|ctx| {
            let globals = ctx.globals();
            let eval_fn: Function = globals.get("__ALBEDO_EVAL_METADATA").map_err(|err| {
                RuntimeError::render(format!(
                    "metadata eval function missing for route '{entry}': {err}"
                ))
            })?;
            let maybe = eval_fn
                .call::<(String, String), MaybePromise>((entry.to_string(), props_json.to_string()))
                .map_err(|err| {
                    RuntimeError::render(format!(
                        "failed to invoke generateMetadata for '{entry}': {err}"
                    ))
                })?;
            maybe.finish::<String>().map_err(|err| {
                RuntimeError::render(format!(
                    "failed to resolve generateMetadata for '{entry}': {err}"
                ))
            })
        });

        if scoped {
            self.runtime.as_ref().expect("runtime initialized").run_gc();
            self.arena.end_request();
        }

        let envelope_json = eval_result?;
        let envelope: MetadataEnvelope = serde_json::from_str(&envelope_json).map_err(|err| {
            RuntimeError::render(format!(
                "failed to decode generateMetadata envelope for '{entry}': {err}"
            ))
        })?;

        if envelope.ok {
            Ok(envelope.value.filter(|value| !value.is_null()))
        } else {
            let message = envelope
                .error
                .unwrap_or_else(|| "unknown generateMetadata error".to_string());
            Err(map_render_error(entry, &message))
        }
    }

    fn ensure_initialized(&mut self) -> RuntimeResult<()> {
        if self.initialized {
            return Ok(());
        }

        let arena = self.arena.clone();
        let runtime = self.runtime.get_or_insert_with(|| {
            Runtime::new_with_alloc(ArenaAllocator::new(arena))
                .expect("QuickJS runtime creation failed")
        });

        if self.context.is_none() {
            self.context = Some(Context::full(runtime).expect("QuickJS context creation failed"));
        }

        let bootstrap = self.bootstrap.take().unwrap_or_default();

        self.context
            .as_ref()
            .unwrap()
            .with(|ctx| -> RuntimeResult<()> {
                // Phase L · install the form-action contract before the
                // helpers that read it. `h()` only dereferences it at render
                // time, so strict ordering isn't load-bearing — but an engine
                // whose shim could observe a half-installed contract would
                // silently render forms without a CSRF input, so the order is
                // pinned rather than left to chance.
                ctx.eval::<(), _>(build_form_contract_script())
                    .map_err(|err| {
                        RuntimeError::init(format!("failed to install form contract: {err}"))
                    })?;

                // Installed alongside the form contract and for the same
                // reason: `h()` must not carry its own copy of a markup rule
                // the pure-Rust renderer also holds.
                ctx.eval::<(), _>(build_markup_contract_script())
                    .map_err(|err| {
                        RuntimeError::init(format!("failed to install markup contract: {err}"))
                    })?;

                ctx.eval::<(), _>(
                    crate::runtime::jsx_attributes::build_jsx_attribute_table_script().as_str(),
                )
                .map_err(|err| {
                    RuntimeError::init(format!(
                        "failed to install the JSX attribute table: {err}"
                    ))
                })?;
                ctx.eval::<(), _>(build_builtin_runtime_helpers_script())
                    .map_err(|err| {
                        RuntimeError::init(format!(
                            "failed to install built-in runtime helpers: {err}"
                        ))
                    })?;

                if !bootstrap.dom_shim_js.trim().is_empty() {
                    ctx.eval::<(), _>(bootstrap.dom_shim_js.as_str())
                        .map_err(|err| {
                            RuntimeError::init(format!("failed to evaluate DOM shim: {err}"))
                        })?;
                }

                if !bootstrap.runtime_helpers_js.trim().is_empty() {
                    ctx.eval::<(), _>(bootstrap.runtime_helpers_js.as_str())
                        .map_err(|err| {
                            RuntimeError::init(format!("failed to evaluate runtime helpers: {err}"))
                        })?;
                }

                ctx.eval::<(), _>("globalThis.__ALBEDO_MODULES = Object.create(null);")
                    .map_err(|err| {
                        RuntimeError::init(format!("failed to initialize module table: {err}"))
                    })?;

                ctx.eval::<(), _>(build_npm_runtime_helpers_script().as_str())
                    .map_err(|err| {
                        RuntimeError::init(format!(
                            "failed to install npm module runtime helpers: {err}"
                        ))
                    })?;

                // The React host records, from the SAME generator the browser
                // runtime uses. A package's `import 'react'` resolves to
                // `albedo:host/react` on both sides, so `forwardRef` returns the
                // same kind of thing in SSR and in hydration — which is what
                // stops a React component library rendering as the literal text
                // `<[object Object]>` here while working perfectly in the
                // browser. Must run AFTER the linker: it writes into
                // `__ALBEDO_NPM_FACTORIES`.
                ctx.eval::<(), _>(
                    crate::runtime::react_host::build_host_module_records_script().as_str(),
                )
                .map_err(|err| {
                    RuntimeError::init(format!("failed to install React host records: {err}"))
                })?;

                let render_script = build_render_function_script();
                ctx.eval::<(), _>(render_script.as_str()).map_err(|err| {
                    RuntimeError::init(format!("failed to install reusable render function: {err}"))
                })?;

                Ok(())
            })?;

        for preload in &bootstrap.preloaded_libraries {
            self.load_module(&preload.specifier, &preload.code)?;
        }

        self.initialized = true;
        Ok(())
    }

    /// [`RuntimeEngine::load_module`], plus the **project-relative** spec this
    /// module's rendered `data-albedo-id` anchors should be keyed to.
    ///
    /// `None` stamps nothing. That is the honest degradation for a module the
    /// caller cannot place in the project: no anchors is what shipped before
    /// this existed, whereas anchors hashed from the wrong string would name
    /// elements the opcode frame never mentions — a silent mis-binding instead
    /// of a visible absence.
    ///
    /// # Errors
    /// As [`RuntimeEngine::load_module`].
    pub fn load_module_with_spec(
        &mut self,
        specifier: &str,
        code: &str,
        stamp_module_spec: Option<&str>,
    ) -> RuntimeResult<()> {
        if code.len() > MAX_MODULE_SIZE {
            return Err(RuntimeError::load(
                LoadErrorKind::EngineFailure,
                format!(
                    "Module '{specifier}' exceeds maximum size limit of {} bytes",
                    MAX_MODULE_SIZE
                ),
            ));
        }

        // The stamp spec is part of the compiled output, so it has to be part of
        // the idempotency key too — otherwise a module warmed without one (the
        // pool's boot warm) would keep its anchorless script forever, and the
        // request path's spec would be silently ignored.
        let mut code_hash = stable_source_hash(code);
        if let Some(spec) = stamp_module_spec {
            code_hash ^= stable_source_hash(spec).rotate_left(1);
        }
        if self.loaded_module_hashes.get(specifier).copied() == Some(code_hash) {
            return Ok(());
        }

        self.ensure_initialized()?;
        let script =
            compile_module_script_for_quickjs_with_spec(specifier, code, stamp_module_spec)?;

        self.context.as_ref().unwrap().with(|ctx| {
            ctx.eval::<(), _>(script.as_str()).map_err(|err| {
                RuntimeError::load(
                    LoadErrorKind::EngineFailure,
                    format!(
                        "failed to load module '{specifier}': {}",
                        describe_js_error(&ctx, &err)
                    ),
                )
            })
        })?;

        self.loaded_module_hashes
            .insert(specifier.to_string(), code_hash);
        Ok(())
    }
}

impl RuntimeEngine for QuickJsEngine {
    fn init(&mut self, bootstrap: &BootstrapPayload) -> RuntimeResult<()> {
        if self.initialized {
            return Ok(());
        }
        self.bootstrap = Some(bootstrap.clone());
        self.ensure_initialized()
    }

    fn load_module(&mut self, specifier: &str, code: &str) -> RuntimeResult<()> {
        self.load_module_with_spec(specifier, code, None)
    }

    fn load_precompiled_module(
        &mut self,
        specifier: &str,
        compiled_script: &str,
        source_hash: u64,
    ) -> RuntimeResult<()> {
        if self.loaded_module_hashes.get(specifier).copied() == Some(source_hash) {
            return Ok(());
        }

        self.ensure_initialized()?;

        self.context.as_ref().unwrap().with(|ctx| {
            ctx.eval::<(), _>(compiled_script).map_err(|err| {
                RuntimeError::load(
                    LoadErrorKind::EngineFailure,
                    format!(
                        "failed to load precompiled module '{specifier}': {}",
                        describe_js_error(&ctx, &err)
                    ),
                )
            })
        })?;

        self.loaded_module_hashes
            .insert(specifier.to_string(), source_hash);
        Ok(())
    }

    fn render_component(&mut self, entry: &str, props_json: &str) -> RuntimeResult<RenderOutput> {
        self.render_component_inner(entry, props_json, None)
    }

    fn render_component_with_host(
        &mut self,
        entry: &str,
        props_json: &str,
        host_json: &str,
    ) -> RuntimeResult<RenderOutput> {
        self.render_component_inner(entry, props_json, Some(host_json))
    }

    fn warm(&mut self) -> RuntimeResult<()> {
        self.ensure_initialized()?;
        self.context.as_ref().unwrap().with(|ctx| {
            ctx.eval::<i32, _>("40 + 2")
                .map(|_| ())
                .map_err(|err| RuntimeError::init(format!("runtime warm-up failed: {err}")))
        })
    }
}

fn build_render_function_script() -> String {
    format!(
        r#"
globalThis.__ALBEDO_RENDER_COMPONENT = function(entry, propsJson, hostJson) {{
  try {{
    // A1 · install the per-render host seed (slot-backed useState values,
    // broadcast-backed useSharedSlot values) and reset the positional hook
    // counter so `useState` pairs with `host.state[idx]`. Empty/absent host
    // means "no seed" — every hook falls back to its initial.
    globalThis.__ALBEDO_HOST = (hostJson && hostJson.length > 0) ? JSON.parse(hostJson) : null;
    globalThis.__ALBEDO_HOOK_INDEX = 0;
    // Anchor ids are per-render, exactly as `render_entry`'s
    // `reset_element_counter()` is on the pure-Rust side — the two counters must
    // start together or every id after the first render drifts.
    if (typeof globalThis.__albedo_reset_element_counter === 'function') {{
      globalThis.__albedo_reset_element_counter();
    }}
    const __albedo_record = globalThis.__ALBEDO_MODULES[entry];
    const __albedo_has_own = Object.prototype.hasOwnProperty;
    const __albedo_is_record = function(candidate) {{
      return candidate !== null
        && typeof candidate === 'object'
        && candidate.{MODULE_RECORD_FLAG} === true;
    }};
    if (typeof __albedo_record === 'undefined') {{
      throw new Error('{MODULE_MISSING_MARKER}' + entry);
    }}
    let __albedo_component = __albedo_record;
    if (__albedo_is_record(__albedo_record)) {{
      if (!__albedo_has_own.call(__albedo_record, 'default')) {{
        throw new Error('{INVALID_ENTRY_EXPORT_MARKER}' + entry);
      }}
      __albedo_component = __albedo_record.default;
    }}
    if (typeof __albedo_component === 'undefined') {{
      throw new Error('{INVALID_ENTRY_EXPORT_MARKER}' + entry);
    }}
    const __albedo_props = JSON.parse(propsJson);
    const __albedo_value = (typeof __albedo_component === 'function')
      ? __albedo_component(__albedo_props, globalThis.__albedo_require)
      : __albedo_component;
    // An async server component (or any thenable-returning render) is awaited on
    // the server: hand the Promise back to the host, which drives the QuickJS job
    // queue to resolution (see `render_component_inner`). The host-seed reset in
    // `finally` stays correct — hooks run during the synchronous prefix, before
    // the component's first `await`, so the seed is consumed before this returns.
    // Synchronous components keep the fast path: a plain string envelope.
    if (__albedo_value !== null
        && (typeof __albedo_value === 'object' || typeof __albedo_value === 'function')
        && typeof __albedo_value.then === 'function') {{
      return __albedo_value.then(
        function(__albedo_resolved) {{
          return JSON.stringify({{ ok: true, value: String(__albedo_resolved) }});
        }},
        function(__albedo_err) {{
          const __albedo_msg = (__albedo_err && typeof __albedo_err.message === 'string')
            ? __albedo_err.message
            : String(__albedo_err);
          return JSON.stringify({{ ok: false, error: __albedo_msg }});
        }}
      );
    }}
    return JSON.stringify({{ ok: true, value: String(__albedo_value) }});
  }} catch (err) {{
    const message = (err && typeof err.message === 'string') ? err.message : String(err);
    return JSON.stringify({{ ok: false, error: message }});
  }} finally {{
    // Never let one render's host seed leak into the next render on this engine.
    globalThis.__ALBEDO_HOST = null;
  }}
}};

// Gate 2 · B slice 3 — evaluate a route module's `generateMetadata(props)`
// export to a plain metadata object (the Next.js `Metadata` shape). Unlike the
// render path this returns DATA, not HTML: the envelope's `value` is the object
// itself (or `null`), JSON-stringified for the host to lower via
// `metadata_from_json`. A route without the export resolves to `null` — benign,
// the static `<head>` stands. Async `generateMetadata` returns a Promise that
// the host drives to settlement, exactly like an async server component.
globalThis.__ALBEDO_EVAL_METADATA = function(entry, propsJson) {{
  try {{
    const __albedo_record = globalThis.__ALBEDO_MODULES[entry];
    if (typeof __albedo_record === 'undefined') {{
      throw new Error('{MODULE_MISSING_MARKER}' + entry);
    }}
    const __albedo_fn = (__albedo_record !== null && typeof __albedo_record === 'object')
      ? __albedo_record.generateMetadata
      : undefined;
    if (typeof __albedo_fn !== 'function') {{
      return JSON.stringify({{ ok: true, value: null }});
    }}
    const __albedo_props = JSON.parse(propsJson);
    const __albedo_value = __albedo_fn(__albedo_props);
    if (__albedo_value !== null
        && (typeof __albedo_value === 'object' || typeof __albedo_value === 'function')
        && typeof __albedo_value.then === 'function') {{
      return __albedo_value.then(
        function(__albedo_resolved) {{
          return JSON.stringify({{ ok: true, value: (__albedo_resolved === undefined ? null : __albedo_resolved) }});
        }},
        function(__albedo_err) {{
          const __albedo_msg = (__albedo_err && typeof __albedo_err.message === 'string')
            ? __albedo_err.message
            : String(__albedo_err);
          return JSON.stringify({{ ok: false, error: __albedo_msg }});
        }}
      );
    }}
    return JSON.stringify({{ ok: true, value: (__albedo_value === undefined ? null : __albedo_value) }});
  }} catch (err) {{
    const message = (err && typeof err.message === 'string') ? err.message : String(err);
    return JSON.stringify({{ ok: false, error: message }});
  }}
}};
"#
    )
}

/// Turn an `rquickjs::Error` into something a human can act on.
///
/// 🪤 **`{err}` on an rquickjs error prints the literal string "Exception
/// generated by QuickJS" and nothing else.** The thrown value stays pending on
/// the context and has to be claimed with [`Ctx::catch`], so every JS failure in
/// this engine — a missing global, a syntax form QuickJS rejects, a package's own
/// `throw` — surfaced through the load path as the *same* sentence.
///
/// Found while measuring npm coverage (`TODO.md` item 9.0): the evaluation stage
/// reported twenty-three distinct failures across the Radix/shadcn layer and
/// every one of them read identically, which makes a count and no diagnosis.
/// This is the same defect item 9.5 names for resolution errors — *"npm package
/// 'crypto' not found in node_modules"* sends a reader hunting for a dependency
/// they never had — one layer down, at execution.
///
/// The message is the exception's own `message`, plus the first stack frame when
/// there is one, because for a compiled npm bundle the frame is the only thing
/// that says *which of 300 files* threw.
fn describe_js_error(ctx: &Ctx<'_>, err: &rquickjs::Error) -> String {
    if !matches!(err, rquickjs::Error::Exception) {
        return err.to_string();
    }

    let value = ctx.catch();
    if let Some(exception) = value.as_exception() {
        let message = exception
            .message()
            .unwrap_or_else(|| "<no message>".to_string());
        // Only the innermost frame: a full QuickJS stack for a bundled package is
        // dozens of lines of factory plumbing, and the throw site is the top one.
        let frame = exception
            .stack()
            .and_then(|stack| stack.lines().next().map(str::trim).map(str::to_string))
            .filter(|frame| !frame.is_empty());
        return match frame {
            Some(frame) => format!("{message} (at {frame})"),
            None => message,
        };
    }

    // A `throw` of a non-Error value — a string, an object, `undefined`. Rare in
    // library code and worth not swallowing, because "threw undefined" is itself
    // the diagnosis.
    match value.try_into_string() {
        Ok(string) => string
            .to_string()
            .unwrap_or_else(|_| "threw a value that will not stringify".to_string()),
        Err(other) => format!("threw a non-Error value ({:?})", other.type_of()),
    }
}

/// A2 · npm dependency runtime — the lazy module linker npm bundles load into.
///
/// Three pieces, installed once per context (right after the
/// `__ALBEDO_MODULES` table):
///
/// 1. **Factory + alias tables.** An npm bundle registers one *factory* per file
///    (`__ALBEDO_NPM_FACTORIES[key] = function(exports) {…}`) and an *alias* per bare specifier
///    (`__ALBEDO_NPM_ALIASES["zod"] = key`). Registration is cheap; nothing runs until first use.
/// 2. **`__albedo_require_record`** — the npm linker. Memoized through `__ALBEDO_MODULES`; the
///    record is **published before the factory body runs**, so import cycles observe a
///    partially-initialized record (Node's CommonJS discipline) instead of recursing forever.
/// 3. **Import-binding helpers** (`__albedo_import_default` / `_namespace` / `_named`) — what
///    compiled `import` statements call. For an npm specifier they apply real ESM semantics
///    (`default` is the `default` property, a namespace/named import sees the record itself). For
///    project modules they fall back to the legacy `__albedo_require`, whose component-aware
///    default unwrapping is preserved byte-for-byte.
///
/// The legacy `__albedo_require` itself moves here as a **global**: compiled
/// module records execute their import statements at *load* time, where the
/// old render-function-local closure was out of scope — which is exactly why a
/// project component importing another project module could not load before.
pub(crate) fn build_npm_runtime_helpers_script() -> String {
    format!(
        "{linker}{quickjs}",
        linker = npm_record_linker_script(),
        quickjs = build_quickjs_module_helpers_script()
    )
}

/// The **portable** half of the npm runtime: the record table, the factory and
/// alias tables, and the lazy memoising linker `__albedo_require_record`.
///
/// 🔑 **Shared verbatim with the browser** (see
/// `bundler::client_npm::build_browser_npm_runtime_script`). Tier C ships npm
/// packages to the client in exactly the server's record format, and the linker
/// that reads that format has to agree byte-for-byte on both sides — a second
/// hand-written copy in `assets/*.js` is the *"three paint-rule
/// implementations"* shape this codebase has already paid for once. There is
/// one implementation and two callers.
///
/// Nothing in here touches QuickJS, the filesystem, or any server capability:
/// it is table lookups and one `Object.create`. The pieces that *are*
/// server-specific — project-module resolution, the component-aware default
/// unwrapping in `__albedo_require` — live in
/// [`build_quickjs_module_helpers_script`] next door.
///
/// The record is published **before** the factory body runs, which is Node's
/// CommonJS cycle discipline: an import cycle observes a partially-initialized
/// record instead of recursing forever, so no topological sort is needed and
/// load order is irrelevant.
pub fn npm_record_linker_script() -> String {
    format!(
        r#"
(function() {{
  if (typeof globalThis.__albedo_require_record === 'function') {{ return; }}
  if (typeof globalThis.__ALBEDO_MODULES === 'undefined') {{
    globalThis.__ALBEDO_MODULES = Object.create(null);
  }}
  globalThis.__ALBEDO_NPM_FACTORIES = Object.create(null);
  globalThis.__ALBEDO_NPM_ALIASES = Object.create(null);
  const __albedo_has_own = Object.prototype.hasOwnProperty;

  globalThis.__albedo_is_npm_module = function(specifier) {{
    const spec = String(specifier);
    return __albedo_has_own.call(globalThis.__ALBEDO_NPM_ALIASES, spec)
      || __albedo_has_own.call(globalThis.__ALBEDO_NPM_FACTORIES, spec);
  }};

  globalThis.__albedo_require_record = function(specifier) {{
    const spec = String(specifier);
    const key = __albedo_has_own.call(globalThis.__ALBEDO_NPM_ALIASES, spec)
      ? globalThis.__ALBEDO_NPM_ALIASES[spec]
      : spec;
    const table = globalThis.__ALBEDO_MODULES;
    if (__albedo_has_own.call(table, key)) {{ return table[key]; }}
    const factory = globalThis.__ALBEDO_NPM_FACTORIES[key];
    if (typeof factory !== 'function') {{
      throw new Error('{MODULE_MISSING_MARKER}' + key);
    }}
    const record = Object.create(null);
    Object.defineProperty(record, '{MODULE_RECORD_FLAG}', {{ value: true, enumerable: false }});
    table[key] = record;
    try {{ factory(record); }} catch (err) {{ delete table[key]; throw err; }}
    return record;
  }};
}})();
"#
    )
}

/// The server-only half: `process`, the legacy `__albedo_require`, the import
/// binding helpers and project-relative module resolution.
fn build_quickjs_module_helpers_script() -> String {
    format!(
        r#"
(function() {{
  if (typeof globalThis.__albedo_require === 'function') {{ return; }}
  if (typeof globalThis.process === 'undefined') {{
    globalThis.process = {{ env: {{ NODE_ENV: 'production' }} }};
  }}
  const __albedo_has_own = Object.prototype.hasOwnProperty;
  const __albedo_is_record = function(candidate) {{
    return candidate !== null
      && typeof candidate === 'object'
      && candidate.{MODULE_RECORD_FLAG} === true;
  }};

  globalThis.__albedo_require = function(specifier) {{
    const resolved = globalThis.__ALBEDO_MODULES[specifier];
    if (typeof resolved === 'undefined') {{
      throw new Error('{MODULE_MISSING_MARKER}' + specifier);
    }}
    if (__albedo_is_record(resolved)) {{
      if (__albedo_has_own.call(resolved, 'default')) {{
        const defaultExport = resolved.default;
        if (typeof defaultExport === 'function') {{
          return function(props) {{ return defaultExport(props, globalThis.__albedo_require); }};
        }}
        return defaultExport;
      }}
      return resolved;
    }}
    if (typeof resolved === 'function') {{
      return function(props) {{ return resolved(props, globalThis.__albedo_require); }};
    }}
    return resolved;
  }};

  globalThis.__albedo_import_default = function(specifier) {{
    if (globalThis.__albedo_is_npm_module(specifier)) {{
      return globalThis.__albedo_require_record(specifier).default;
    }}
    return globalThis.__albedo_require(specifier);
  }};
  globalThis.__albedo_import_namespace = function(specifier) {{
    if (globalThis.__albedo_is_npm_module(specifier)) {{
      return globalThis.__albedo_require_record(specifier);
    }}
    return globalThis.__albedo_require(specifier);
  }};
  globalThis.__albedo_import_named = globalThis.__albedo_import_namespace;

  // Project-relative import resolution. A compiled `import … from "../x"`
  // can't look the dependency up by its as-written source: project modules
  // register in `__ALBEDO_MODULES` under their absolute `module_path` key
  // (`A:\proj\src\components\X.tsx`), not the relative specifier. The rewriter
  // collapses the relative path against the importer to an extensionless base
  // (forward-slashed) and hands it here; we recover the registered key by
  // probing the same extension candidates the scanner uses, matching on a
  // slash-normalized form so a `\`-keyed table still resolves. A miss returns
  // the base unchanged so `__albedo_require` throws a loud MODULE_MISSING
  // naming it. (Bare/npm specifiers never reach here — they aren't wrapped.)
  globalThis.__albedo_resolve_project = function(base) {{
    const table = globalThis.__ALBEDO_MODULES;
    if (__albedo_has_own.call(table, base)) {{ return base; }}
    const norm = function(s) {{ return String(s).replace(/\\/g, '/'); }};
    const want = norm(base);
    const exts = ['.tsx', '.jsx', '.js', '.ts'];
    const candidates = [want];
    for (let i = 0; i < exts.length; i++) {{ candidates.push(want + exts[i]); }}
    for (let i = 0; i < exts.length; i++) {{ candidates.push(want + '/index' + exts[i]); }}
    // Prefer an exact (normalized) key match; fall back to a path-suffix match
    // so a relative-keyed importer still resolves against an absolute key.
    let suffixHit = null;
    for (const key in table) {{
      const nk = norm(key);
      for (let i = 0; i < candidates.length; i++) {{
        const c = candidates[i];
        if (nk === c) {{ return key; }}
        if (suffixHit === null && nk.endsWith('/' + c)) {{ suffixHit = key; }}
      }}
    }}
    return suffixHit !== null ? suffixHit : base;
  }};
}})();
"#
    )
}

/// Phase L · hands the form-action markup contract to the JS side as
/// data.
///
/// The `h()` shim below has to perform the same `action="action:NAME"`
/// rewrite the pure-Rust renderer does, including emitting the hidden
/// CSRF input. Restating those literals in JS is exactly how the two
/// renderers drifted the first time (a Tier-B form shipped with no CSRF
/// input at all, and the gate that keys off its presence let the submit
/// through). So the constants cross the language boundary as *values*
/// from `transforms::form`, and the shim reads them — there is one
/// spelling, in Rust, and JS cannot disagree with it.
///
/// `serde_json` does the encoding: its string output is valid JS source
/// for the same literal, so a quote or backslash in a constant can't
/// break out into the surrounding script.
fn build_form_contract_script() -> String {
    use crate::transforms::form::{
        ACTION_ENDPOINT_PREFIX, CSRF_PLACEHOLDER_INPUT, FORM_ACTION_ATTR, FORM_ACTION_PREFIX,
        FORM_HIDDEN_INPUTS,
    };
    // Infallible: `serde_json` cannot fail to encode a `&str`.
    let js = |value: &str| {
        serde_json::to_string(value).expect("encoding a &str as a JSON string cannot fail")
    };
    format!(
        "globalThis.__ALBEDO_FORM_CONTRACT = {{ prefix: {}, attr: {}, csrfInput: {}, \
         endpointPrefix: {}, hiddenInputs: {} }};",
        js(FORM_ACTION_PREFIX),
        js(FORM_ACTION_ATTR),
        js(CSRF_PLACEHOLDER_INPUT),
        js(ACTION_ENDPOINT_PREFIX),
        js(FORM_HIDDEN_INPUTS),
    )
}

/// Hands the JS shim the two markup rules it cannot be trusted to restate:
/// which tags are void, and what a `<children />` sink lowers to.
///
/// Same reasoning as [`build_form_contract_script`], and the same failure it
/// prevents. Before this existed the shim closed *every* tag, so a Tier-B
/// `<hr />` shipped as `<hr></hr>` while the Tier-A render of the same source
/// shipped `<hr />` — two renderers, one component, different bytes. The stray
/// end tag is inert in a browser but not in the string-level row templating
/// this codebase does downstream, which had already grown a workaround for it
/// (`shared_slot_lists`' "void elements with explicit close tags do not
/// truncate a row"). And `<children />` was worse than a byte difference: the
/// pure-Rust renderer lowers it to the sentinel `wrap_in_layouts` substitutes,
/// so a layout rendered through QuickJS emitted a literal `<children></children>`
/// element and dropped the entire page body it was supposed to receive.
///
/// Both constants cross as *values* from Rust. There is one spelling of each
/// and JS cannot disagree with it.
fn build_markup_contract_script() -> String {
    use crate::runtime::eval::component::HTML_VOID_ELEMENTS;
    use crate::runtime::eval::LAYOUT_CHILDREN_SENTINEL;
    // Infallible: `serde_json` cannot fail to encode `&str` / `&[&str]`.
    let json = |value: &serde_json::Value| {
        serde_json::to_string(value).expect("encoding string data as JSON cannot fail")
    };
    let void = serde_json::Value::Array(
        HTML_VOID_ELEMENTS
            .iter()
            .map(|tag| serde_json::Value::String((*tag).to_string()))
            .collect(),
    );
    // The unitless-CSS set crosses for the same reason the void tags do. It is
    // the one part of React's style rule that is *data* rather than algorithm,
    // and it is the part with a browser-visible failure mode: a second copy in
    // JS that fell behind would emit `flex:1px`, which the browser discards, on
    // a component whose Tier-A render said `flex:1`.
    let mut unitless: Vec<&String> = crate::runtime::eval::component::css_unitless_properties()
        .iter()
        .collect();
    unitless.sort();
    let unitless = serde_json::Value::Array(
        unitless
            .into_iter()
            .map(|name| serde_json::Value::String(name.clone()))
            .collect(),
    );
    format!(
        "globalThis.__ALBEDO_MARKUP_CONTRACT = {{ voidTags: {}, layoutChildren: {}, \
         styleUnitless: {} }};",
        json(&void),
        json(&serde_json::Value::String(
            LAYOUT_CHILDREN_SENTINEL.to_string()
        )),
        json(&unitless),
    )
}

fn build_builtin_runtime_helpers_script() -> &'static str {
    r#"
if (typeof globalThis.h !== 'function') {
  // Text content and attribute values are escaped DIFFERENTLY, and both must
  // match `runtime::eval::component`'s `escape_html` / `escape_attr` exactly.
  //
  // A single over-escaping function served both for a long time. It is safe —
  // over-escaping never under-escapes — but it is not *equal*, and equality is
  // the property that matters here: a `"` in text came out as `&quot;` from
  // QuickJS and as `"` from the pure-Rust renderer. Same page in a browser,
  // different bytes, and the bytes are what the row-delta path compares. A row
  // rendered once by each renderer would read as changed when nothing had
  // changed, and it cost five bytes per quote on every wire that carried one.
  //
  // Dropping `'` and `"` from the *text* escape is safe because neither can
  // terminate a text node; dropping `'` from the *attribute* escape is safe
  // because every attribute here is emitted double-quoted. `&`, `<` and `>` are
  // escaped in both, which is what actually prevents breaking out.
  const __albedo_escape_text = function(str) {
    return String(str).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  };

  const __albedo_escape_attr = function(str) {
    return __albedo_escape_text(str).replace(/"/g, '&quot;');
  };

  // Marker type for HTML strings produced by h() that are already safe to
  // embed verbatim. Plain user values (strings, numbers) passed as JSX
  // expression children are NOT this type and must be escaped before use.
  function AlbedoHtml(str) { this.v = str; }
  AlbedoHtml.prototype.toString = function() { return this.v; };

  const __albedo_push_children = function(value, out) {
    if (Array.isArray(value)) {
      for (const item of value) {
        __albedo_push_children(item, out);
      }
      return;
    }
    if (value === null || typeof value === 'undefined' || value === false) {
      return;
    }
    // Output of a prior h() call — already-safe markup, pass through verbatim.
    if (value instanceof AlbedoHtml) {
      out.push(value);
      return;
    }
    // Plain user value (string, number, …) — escape before embedding in HTML.
    out.push(new AlbedoHtml(__albedo_escape_text(String(value))));
  };

  // Framework-level props that are never HTML attributes: `key` (React's
  // reconciliation identity), `ref` (host-node escape hatch), and `children`
  // (rendered between the tags, not on them). None is valid HTML.
  //
  // MUST match the Rust-side `is_reserved_jsx_prop` in `runtime::eval::component`
  // — this is the same rule on the far side of a language boundary, and if the
  // two lists drift, a component's SSR markup stops matching its client markup.
  const __albedo_is_reserved_prop = function(name) {
    return name === 'key' || name === 'ref' || name === 'children';
  };

  // Phase L · `<form action="action:NAME">` detection. The literals come from
  // `__ALBEDO_FORM_CONTRACT`, injected from the Rust constants in
  // `transforms::form` that the pure-Rust renderer emits from — so a Tier-B
  // form and a Tier-A form are the same markup by construction rather than by
  // two lists someone has to remember to keep in step.
  //
  // Returns the bare action name, or null for a plain HTML `<form>` (which must
  // pass through untouched and keep its native submit behaviour).
  const __albedo_form_action_name = function(type, props) {
    if (type !== 'form') {
      return null;
    }
    const contract = globalThis.__ALBEDO_FORM_CONTRACT;
    if (!contract) {
      return null;
    }
    const action = props.action;
    if (typeof action !== 'string' || action.indexOf(contract.prefix) !== 0) {
      return null;
    }
    return action.slice(contract.prefix.length);
  };

  // The URL a form-action `<form>` actually posts to when no client runtime
  // ever ran, or null when the action name cannot be a path segment.
  //
  // Mirrors `transforms::form::action_endpoint`. The prefix crosses from Rust
  // on the contract; the alphabet is restated here because a byte predicate has
  // no JSON representation — so the parity is pinned behaviourally instead, by
  // `a_form_action_name_that_cannot_be_a_url_segment_ships_no_action_attribute`
  // rendering through this shim and asserting the same refusal the pure-Rust
  // renderer makes. `.` is excluded on both sides: allowing it allows `..`.
  const __albedo_action_endpoint = function(name) {
    if (typeof name !== 'string' || name.length === 0 || name.length > 64) {
      return null;
    }
    if (!/^[A-Za-z0-9_-]+$/.test(name)) {
      return null;
    }
    const contract = globalThis.__ALBEDO_FORM_CONTRACT;
    if (!contract || typeof contract.endpointPrefix !== 'string') {
      return null;
    }
    return contract.endpointPrefix + name;
  };

  // Whether a plain `<form>` — no `action:` sentinel — still earns the hidden
  // inputs. Mirrors `transforms::form::plain_form_needs_hidden_inputs`, which
  // carries the reasoning: POST only (a GET would put the token in the URL),
  // same-origin only (an absolute action would hand the token to a third
  // party). Parity with the pure-Rust renderer is pinned behaviourally by
  // `a_plain_post_form_gets_the_hidden_inputs_through_the_shim`.
  const __albedo_plain_form_needs_hidden_inputs = function(type, props) {
    if (type !== 'form') {
      return false;
    }
    const method = props.method;
    if (typeof method !== 'string' || method.trim().toLowerCase() !== 'post') {
      return false;
    }
    const action = props.action;
    if (action === undefined || action === null || action === '') {
      return true;
    }
    if (typeof action !== 'string') {
      return false;
    }
    const trimmed = action.trim();
    if (trimmed === '') {
      return true;
    }
    return trimmed.charAt(0) === '/' && trimmed.slice(0, 2) !== '//';
  };

  // Phase P · Stream E.1 — `<children />` is the layout-wrap intrinsic, not a
  // host element. It lowers to the sentinel comment that the manifest builder's
  // `wrap_in_layouts` substitutes the inner page HTML into. Intercepted here,
  // ahead of the host-element path, exactly as the pure-Rust renderer does it —
  // otherwise a layout rendered through QuickJS emits a literal
  // `<children></children>`, nothing ever substitutes it, and the page body the
  // layout was supposed to wrap is silently dropped.
  const __albedo_layout_children_sentinel = function() {
    const contract = globalThis.__ALBEDO_MARKUP_CONTRACT;
    return contract && contract.layoutChildren ? contract.layoutChildren : '';
  };

  // Void elements take no closing tag. The set crosses from Rust on
  // `__ALBEDO_MARKUP_CONTRACT` so it cannot drift from `is_void_tag`.
  const __albedo_is_void_tag = function(tag) {
    const contract = globalThis.__ALBEDO_MARKUP_CONTRACT;
    if (!contract || !contract.voidTags) {
      return false;
    }
    return contract.voidTags.indexOf(tag) !== -1;
  };

  // React's style rule, mirroring `runtime::eval::component`'s
  // `style_object_to_css` / `hyphenate_style_name` / `style_value_to_css`.
  //
  // The *set* of unitless properties is not restated here — it crosses from
  // Rust on `__ALBEDO_MARKUP_CONTRACT` so there is one spelling of it. The
  // transform itself is algorithm, not data, and is duplicated in the same way
  // `__albedo_escape_attr` duplicates `escape_attr`: the conformance fixture
  // `render_quickjs/style_object` is what holds the two implementations equal.
  const __albedo_is_unitless_style = function(property) {
    const contract = globalThis.__ALBEDO_MARKUP_CONTRACT;
    if (!contract || !contract.styleUnitless) {
      return false;
    }
    return contract.styleUnitless.indexOf(property) !== -1;
  };

  const __albedo_hyphenate_style_name = function(name) {
    let out = '';
    for (let i = 0; i < name.length; i++) {
      const ch = name.charAt(i);
      if (ch >= 'A' && ch <= 'Z') {
        out += '-' + ch.toLowerCase();
      } else {
        out += ch;
      }
    }
    // `msTransform` hyphenates to `ms-transform`, but CSS wants `-ms-transform`.
    return out.indexOf('ms-') === 0 ? '-' + out : out;
  };

  const __albedo_style_to_css = function(style) {
    let out = '';
    // `for…in` walks a JS object in insertion order for string keys, which is
    // the authored order of the object literal — the same order the pure-Rust
    // side reconstructs from the AST.
    for (const name in style) {
      if (!Object.prototype.hasOwnProperty.call(style, name)) {
        continue;
      }
      const value = style[name];
      let rendered;
      if (value === null || typeof value === 'undefined' || typeof value === 'boolean') {
        continue;
      } else if (typeof value === 'number') {
        // `0` never takes a unit; nor do custom properties or the unitless set.
        rendered = String(value);
        if (value !== 0 && name.indexOf('--') !== 0 && !__albedo_is_unitless_style(name)) {
          rendered += 'px';
        }
      } else {
        rendered = String(value).trim();
        if (rendered === '') {
          continue;
        }
      }
      const property = name.indexOf('--') === 0 ? name : __albedo_hyphenate_style_name(name);
      if (out !== '') {
        out += ';';
      }
      out += property + ':' + rendered;
    }
    return out;
  };

  const h = function(type, props, ...children) {
    const flatChildren = [];
    __albedo_push_children(children, flatChildren);

    if (type === 'children') {
      // A content sink, not a wrapper: attrs and children are ignored, matching
      // the pure-Rust intrinsic.
      return new AlbedoHtml(__albedo_layout_children_sentinel());
    }

    if (typeof type === 'function') {
      const mergedProps = Object.assign({}, props || {});
      if (flatChildren.length === 1) {
        mergedProps.children = flatChildren[0];
      } else if (flatChildren.length > 1) {
        mergedProps.children = flatChildren;
      }
      return type(mergedProps);
    }

    // A component type that is neither a function nor a tag name must NOT fall
    // through to the element branch below — that interpolates the object into a
    // tag name and emits the literal text `<[object Object]>` into the page.
    //
    // 🪤 This used to fire for every React component library, because a package
    // bound to the real `react` in `node_modules` and `React.forwardRef` returns
    // an *object*. That cause is gone — `runtime::react_host` binds a package's
    // react to this runtime on both sides — and the guard stays for whatever
    // finds the next way in. Visible corruption in someone's HTML is the one
    // outcome worse than a named failure.
    if (typeof type !== 'string') {
      throw new Error(
        '[albedo] cannot server-render a component whose type is ' +
        (type === null ? 'null' : typeof type) +
        '. A component must be a function or a tag name; an object here is ' +
        'usually a foreign framework\'s element wrapper that Albedo does not model.'
      );
    }

    let attrs = '';
    const safeProps = props || {};
    const formAction = __albedo_form_action_name(type, safeProps);
    const formActionEndpoint = formAction === null ? null : __albedo_action_endpoint(formAction);
    for (const key in safeProps) {
      if (!Object.prototype.hasOwnProperty.call(safeProps, key)) {
        continue;
      }
      // React's `key` is not an HTML attribute, but it IS the delta sink's
      // reconciliation identity. Stamp it as `data-albedo-key` so a keyed list's
      // Tier-B rows can be reconciled by the client sink — the QuickJS mirror of
      // Phase-K's `stamp_row_key`. `ref`/`children` stay stripped: they carry no
      // identity. (This is a deliberate, list-scoped divergence from the plain
      // `is_reserved_jsx_prop` strip; a stray non-list `key` just yields an inert
      // attribute.)
      if (key === 'key') {
        const keyVal = safeProps.key;
        if (keyVal !== false && keyVal !== null
            && typeof keyVal !== 'undefined' && typeof keyVal !== 'function') {
          attrs += ' data-albedo-key="' + __albedo_escape_attr(keyVal) + '"';
        }
        continue;
      }
      if (__albedo_is_reserved_prop(key)) {
        continue;
      }
      // The sentinel `action` attribute is consumed by the rewrite below and
      // must not also ship as a literal `action="action:NAME"` — that would
      // make the form navigate to a bogus URL if the client runtime never
      // loaded, instead of simply doing nothing.
      if (formAction !== null && key === 'action') {
        continue;
      }
      // An authored `method` on a form we are giving a real endpoint to is
      // overwritten below, not honoured: an action is a mutation, and a GET
      // form would put every field in the URL bar, the history and the access
      // log. Skipped here only when there IS an endpoint, so a form whose name
      // could not become one keeps whatever the author wrote.
      if (formActionEndpoint !== null && key === 'method') {
        continue;
      }
      const value = safeProps[key];
      if (value === false || value === null || typeof value === 'undefined') {
        continue;
      }
      // Event handlers (`onClick={fn}`) and any other function-valued prop are
      // not HTML attributes — dropping them keeps server markup clean. The
      // client-side binding for these is carried by the Phase K opcode stream,
      // not by stringifying the closure into the tag.
      if (typeof value === 'function') {
        continue;
      }
      // One table, generated from `runtime::jsx_attributes` and installed by
      // `build_jsx_attribute_table_script`. Not a ternary chain here and a
      // `match` in the pure-Rust renderer: the two are required to agree
      // byte-for-byte (see the void-element spelling below), and two hand-kept
      // lists of the same rule is how they stop agreeing.
      const attrName = globalThis.__albedo_attr_name(key);
      if (value === true) {
        attrs += ' ' + attrName;
        continue;
      }
      // `style` is an object in JSX and CSS text in HTML. Without this it fell
      // through to `String(value)` and shipped `style="[object Object]"`.
      if (attrName === 'style' && typeof value === 'object') {
        const css = __albedo_style_to_css(value);
        if (css !== '') {
          attrs += ' style="' + __albedo_escape_attr(css) + '"';
        }
        continue;
      }
      attrs += ' ' + attrName + '="' + __albedo_escape_attr(value) + '"';
    }

    // Phase L · stamp the real endpoint, the rewritten action hook, and the
    // hidden inputs as the form's first children — byte-identical to what the
    // pure-Rust renderer emits, because both read the same constants and emit
    // them in the same order (`action`, `method`, `data-albedo-action`).
    //
    // Both the URL and the attribute, not either: the attribute is what the
    // client interceptor keys on, the URL is what the browser uses when no
    // interceptor ran. The inputs ship empty; the server fills the per-session
    // token and the request's own path at request time
    // (`fill_csrf_tokens` / `fill_return_paths`).
    let inner = flatChildren.join('');
    if (formAction !== null) {
      const contract = globalThis.__ALBEDO_FORM_CONTRACT;
      if (formActionEndpoint !== null) {
        attrs += ' action="' + __albedo_escape_attr(formActionEndpoint) + '"';
        attrs += ' method="post"';
      }
      attrs += ' ' + contract.attr + '="' + __albedo_escape_attr(formAction) + '"';
      inner = (formActionEndpoint !== null ? contract.hiddenInputs : contract.csrfInput) + inner;
      // P6 · append this action's per-field `data-albedo-error` spans — the
      // sinks the submit projection's `SetText` targets to clear/fill
      // validation messages. The pure-Rust renderer interleaves them after
      // each field via a render-time scope stack; this shim is bottom-up
      // (children are already stringified before the form runs), so it can't
      // see which fields belong to the form and instead appends the whole
      // set at the form's end. The markup + ids come from the server
      // (`form_error_span_seed`, same `allocate_field_error_id` the pure-Rust
      // path uses), so the nodes exist with the exact ids the projection
      // addresses — bakabox no longer hits a missing node and drops the
      // frame. Placement differs from Tier-A only for a *visible* error
      // message; on the success path (all spans cleared to empty) it is
      // indistinguishable.
      const host = globalThis.__ALBEDO_HOST;
      const spans = host && host.formErrorSpans ? host.formErrorSpans[formAction] : null;
      if (typeof spans === 'string') {
        inner = inner + spans;
      }
    } else if (__albedo_plain_form_needs_hidden_inputs(type, safeProps)) {
      // A plain same-origin POST form — the sign-in forms are the first
      // instance. No `data-albedo-action` and no rewritten `action`: the
      // author's URL is the one that posts, and there is nothing for the
      // client interceptor to key on. It gets the hidden inputs and nothing
      // else.
      inner = globalThis.__ALBEDO_FORM_CONTRACT.hiddenInputs + inner;
    }
    // Void elements close themselves. The ` />` spelling (and the space before
    // it) is the pure-Rust renderer's, so the two agree byte-for-byte rather
    // than merely parsing to the same tree.
    if (inner === '' && __albedo_is_void_tag(String(type))) {
      return new AlbedoHtml('<' + String(type) + attrs + ' />');
    }
    return new AlbedoHtml('<' + String(type) + attrs + '>' + inner + '</' + String(type) + '>');
  };

  h.Fragment = function Fragment(fragmentProps) {
    if (!fragmentProps || typeof fragmentProps.children === 'undefined') {
      return new AlbedoHtml('');
    }
    const out = [];
    __albedo_push_children(fragmentProps.children, out);
    return new AlbedoHtml(out.join(''));
  };

  // Island-boundary primitive (server render context). A Tier-C island reached
  // from a server-rendered (Tier-B/async) parent is compiled to a *client
  // reference* whose module body is a stub that returns THIS — the framework's
  // canonical empty island placeholder — instead of executing island code. The
  // string is byte-identical to what the pure-Rust renderer emits for a Tier-A
  // parent's island child (`eval::core`), so a single serve-time fill pass
  // replaces it with the island's SSR markup + `data-albedo-island` marker for
  // every island uniformly, regardless of which renderer emitted the hole.
  globalThis.__albedo_island_placeholder = function(placeholderId) {
    return new AlbedoHtml('<div id="' + placeholderId + '" data-albedo-tier="c"></div>');
  };

  // Shell-stamped anchor ids, the QuickJS half of `eval::core`'s
  // `next_element_stable_id`. Same function of the same inputs:
  // `fnv1a_32("{module_spec}#{counter}")`, one shared counter per render,
  // advanced in pre-order.
  //
  // Pre-order falls out of JS argument evaluation: JSX lowers to nested
  // `h(type, props, ...children)`, and a parent's props object — where the
  // transform put this call — is built before any child's `h(…)` runs. That is
  // why the stamp is injected as a JSX attribute rather than added inside `h`,
  // which by then has already stringified its children and would number the
  // parent last.
  let __albedo_element_counter = 0;
  globalThis.__albedo_reset_element_counter = function() {
    __albedo_element_counter = 0;
  };
  globalThis.__albedo_stable_id = function(moduleSpec) {
    const counter = __albedo_element_counter++;
    const key = String(moduleSpec) + '#' + counter;
    // FNV-1a over UTF-8 BYTES, matching `fnv1a_32(key.as_bytes())`. Specs are
    // ASCII in practice, but a project under a non-ASCII path would otherwise
    // hash differently on each side and silently unbind every element in it.
    let hash = 0x811c9dc5 >>> 0;
    for (let i = 0; i < key.length; i++) {
      const code = key.charCodeAt(i);
      if (code < 0x80) {
        hash = Math.imul((hash ^ code) >>> 0, 16777619) >>> 0;
        continue;
      }
      const point = key.codePointAt(i);
      if (point > 0xffff) i++;
      const bytes = point < 0x800
        ? [0xc0 | (point >> 6), 0x80 | (point & 0x3f)]
        : point < 0x10000
          ? [0xe0 | (point >> 12), 0x80 | ((point >> 6) & 0x3f), 0x80 | (point & 0x3f)]
          : [0xf0 | (point >> 18), 0x80 | ((point >> 12) & 0x3f),
             0x80 | ((point >> 6) & 0x3f), 0x80 | (point & 0x3f)];
      for (let b = 0; b < bytes.length; b++) {
        hash = Math.imul((hash ^ bytes[b]) >>> 0, 16777619) >>> 0;
      }
    }
    return String(hash >>> 0);
  };

  // Phase L · `<Link href>` on the QuickJS render path.
  //
  // `<Link>` is a compile-time component: `eval::core` rewrites the TAG to `a`
  // and pushes a bare `data-albedo-link` attribute. That rewrite lives in the
  // pure-Rust evaluator's JSX walker, and `transforms::link` is explicitly a
  // metadata-only pass that "does not rewrite the AST" — so nothing ever
  // rewrote it for THIS engine. JSX lowers `<Link>` to `h(Link, …)`, `Link`
  // was an undefined free identifier, and any component containing one threw
  // `Link is not defined` the moment it rendered through QuickJS.
  //
  // Why that was invisible: a route renders through the pure-Rust evaluator,
  // which handles `<Link>` fine. The QuickJS path is reached when an island is
  // rendered STANDALONE from its module path — and that call site
  // (`renderer_runtime::render_island_html`) catches the error, logs a
  // `tracing::warn!` nobody sees, and leaves the placeholder empty. The island
  // then has no `data-albedo-island` marker, so the interaction-triggered
  // bootstrap can never find a node to hydrate and the component never mounts.
  // A navigation bar with one `<Link>` in it disappeared from every page of a
  // real site with a green build and a clean console.
  //
  // Defined as a function component so `h`'s existing `typeof type ===
  // 'function'` branch invokes it. `data-albedo-link` is assigned LAST, after
  // `children` is removed, so the emitted attribute order matches
  // `eval::core` — which pushes it after the authored attrs. That parity is
  // load-bearing: the row-delta path compares the two renderers' bytes, and a
  // Tier-A `<Link>` and a Tier-C `<Link>` must agree exactly.
  globalThis.Link = function(props) {
    const merged = Object.assign({}, props || {});
    const children = merged.children;
    delete merged.children;
    delete merged['data-albedo-link'];
    merged['data-albedo-link'] = true;
    return h('a', merged, children);
  };

  globalThis.h = h;

  // The one export in the shared React host table whose implementation cannot
  // be shared: an "element" is an `AlbedoHtml` here and a vnode in the browser.
  // Routing `isValidElement` through this global keeps the table itself
  // single-sourced (`runtime::react_host`) and reduces the difference between
  // the two runtimes to one named function.
  globalThis.__albedo_is_element = function(value) {
    return value instanceof AlbedoHtml;
  };
}

// A1 · host-object bridge (render side). The framework hooks resolve here
// instead of through `__albedo_require("react"|"albedo")` so a real TSX hook
// component LOADS and RENDERS under QuickJS. Their values come from a
// per-render host seed (`globalThis.__ALBEDO_HOST`) the renderer installs
// just before invoking the component:
//
//   { state: { "<hookIdx>": <currentValue>, ... },   // useState, slot-backed
//     shared: { "<topic>": <currentValue>, ... } }    // useSharedSlot, broadcast-backed
//
// `useState` is positional like React: a per-render index counter
// (`__ALBEDO_HOOK_INDEX`, reset by the render entry) pairs the Nth call with
// `state["N"]`. An index the seed doesn't carry falls back to the call's own
// initial argument, so an unwritten slot renders its initial — parity with the
// pure-Rust `render_local` seeding. The setter is a no-op: a single SSR pass
// has a fixed state snapshot; mutations travel client→server as actions.
if (typeof globalThis.useState !== 'function') {
  globalThis.__ALBEDO_HOST = null;
  globalThis.__ALBEDO_HOOK_INDEX = 0;

  const __albedo_has_own = Object.prototype.hasOwnProperty;

  globalThis.useState = function(initial) {
    const index = globalThis.__ALBEDO_HOOK_INDEX++;
    const host = globalThis.__ALBEDO_HOST;
    let value = initial;
    if (host && host.state && __albedo_has_own.call(host.state, String(index))) {
      value = host.state[String(index)];
    }
    const setState = function() { /* SSR render: state is fixed for this pass */ };
    return [value, setState];
  };

  globalThis.useSharedSlot = function(topic) {
    const host = globalThis.__ALBEDO_HOST;
    const key = String(topic);
    if (host && host.shared && __albedo_has_own.call(host.shared, key)) {
      return host.shared[key];
    }
    return null;
  };

  // PRISM · a partitioned topic (`messages.where({ room: params.id })`) is not
  // knowable until a request supplies the key, so the transpile rewrites the
  // argument to this lookup and the render seeds `host.topics` by binding name
  // after resolving the spec against the route's params. Resolution lives in
  // Rust — the same resolver the subscribe path uses — so nothing mints a topic
  // string here. An unresolved binding yields null, which `useSharedSlot`
  // stringifies to a key `host.shared` will not have: the slot reads null, the
  // page renders, nothing throws.
  globalThis.__albedo_topic = function(binding) {
    const host = globalThis.__ALBEDO_HOST;
    const key = String(binding);
    if (host && host.topics && __albedo_has_own.call(host.topics, key)) {
      return host.topics[key];
    }
    return null;
  };

  // B4 · the paint rule for a stamped scalar shared-slot read. The marker
  // (`transforms::shared_slot_lists`) folds every anchored read through this,
  // so what SSR writes into the holder is what a later `SlotSet` will write
  // over it — the client applies the same table to the topic's JSON.
  //
  // Only anchored reads are folded, so this does not change how JSX coerces
  // anything else. The table:
  //
  //   null / undefined -> ""          (matches `h`'s child skip)
  //   string           -> the string  (unquoted — the SSR behaviour, kept)
  //   number / boolean -> String(v)
  //   anything else    -> JSON        (was `[object Object]`, and `123` for
  //                                    the array [1,2,3])
  //
  // JSON for the object case rather than `String(v)` because the live wire
  // carries the topic's JSON encoding: agreeing on `[object Object]` would
  // mean shipping a value the client cannot paint back, and agreeing on
  // anything else would mean a second encoding on the wire.
  // `path` is the dotted projection the holder also carries as
  // `data-albedo-slot-path`, present only for `<span>{status.state}</span>`.
  // Walking it here rather than leaving `status.state` in the tree is what
  // keeps an unresolved topic from throwing: `useSharedSlot` answers null for a
  // topic this request had no value for, and `null.state` would take the route
  // down instead of rendering empty and going live when the value arrives.
  globalThis.__albedo_slot_text = function(value, path) {
    if (typeof path === 'string' && path.length > 0) {
      const segments = path.split('.');
      for (let i = 0; i < segments.length; i++) {
        if (value === null || typeof value !== 'object') { return ''; }
        value = value[segments[i]];
      }
    }
    if (value === null || typeof value === 'undefined') { return ''; }
    if (typeof value === 'string') { return value; }
    if (typeof value === 'number' || typeof value === 'boolean') { return String(value); }
    let text;
    try {
      text = JSON.stringify(value);
    } catch (err) {
      // A cycle, or a `toJSON` that throws. A render must not die over the
      // formatting of one text node.
      return '';
    }
    // `JSON.stringify` answers `undefined` for a function or a symbol, and
    // `String(undefined)` downstream would paint the word "undefined".
    return typeof text === 'string' ? text : '';
  };

  // Server-side no-ops / pass-throughs so a component using the rest of the
  // hook surface neither fails to load nor crashes mid-render. Effects never
  // run during SSR; refs/memo/callback return shapes the render can read.
  globalThis.useEffect = function() {};
  globalThis.useLayoutEffect = function() {};
  globalThis.useRef = function(initial) {
    return { current: (initial === undefined ? null : initial) };
  };
  globalThis.useMemo = function(factory) {
    return (typeof factory === 'function') ? factory() : undefined;
  };
  globalThis.useCallback = function(fn) { return fn; };

  // Context. SSR `h` invokes components EAGERLY (children are already-rendered
  // HTML before a Provider runs), so a Provider cannot thread its value down to
  // nested consumers in this single pass — that propagation is applied by the
  // client runtime on hydration. Here `useContext` returns a renderer-seeded
  // value (`host.context[id]`) when present, else the context default, so a
  // component using context LOADS and RENDERS without crashing. The Provider is
  // a transparent pass-through that renders its children.
  globalThis.__albedo_context_seq = 0;
  globalThis.createContext = function(defaultValue) {
    const id = ++globalThis.__albedo_context_seq;
    const Provider = function(props) {
      return (props && typeof props.children !== 'undefined') ? props.children : '';
    };
    Provider.__albedoContextId = id;
    return { __albedoContext: true, _id: id, _defaultValue: defaultValue, Provider: Provider };
  };
  globalThis.useContext = function(context) {
    const host = globalThis.__ALBEDO_HOST;
    if (host && host.context && context && __albedo_has_own.call(host.context, String(context._id))) {
      return host.context[String(context._id)];
    }
    return context ? context._defaultValue : undefined;
  };

  // `export const X = action(fn)` runs `action(fn)` at module load. Keep it a
  // benign pass-through so loading the module never throws; the action body
  // itself dispatches through the QuickJS action bridge, not this render path.
  globalThis.action = function(fn) { return fn; };
  // A `broadcast(...)` reached during render (rare) is a no-op here — render is
  // read-only; writes happen in the action bridge.
  if (typeof globalThis.broadcast !== 'function') {
    globalThis.broadcast = function() {};
  }
}
"#
}

pub(crate) fn compile_module_script_for_quickjs(
    specifier: &str,
    code: &str,
) -> RuntimeResult<String> {
    compile_module_script_for_quickjs_with_spec(specifier, code, None)
}

/// [`compile_module_script_for_quickjs`] with the **project-relative** module
/// spec the rendered markup's `data-albedo-id`s should be keyed to.
///
/// Separate from `specifier` on purpose: the engine keys modules by the path
/// imports resolve against (absolute, machine-specific), while an anchor id must
/// hash the same string the pure-Rust renderer hashes, or the opcode frame and
/// the DOM name different elements. `None` stamps nothing — the Tier-C island
/// build and any caller with no project context.
pub(crate) fn compile_module_script_for_quickjs_with_spec(
    specifier: &str,
    code: &str,
    stamp_module_spec: Option<&str>,
) -> RuntimeResult<String> {
    let normalized = code.trim();
    if normalized.is_empty() {
        return Err(RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!("module '{specifier}' is empty"),
        ));
    }

    let transpiled =
        transpile_module_source_for_quickjs(specifier, normalized, stamp_module_spec)?;

    if !transpiled.contains("export") && !transpiled.contains("import") {
        return compile_legacy_expression_module(specifier, transpiled.as_str());
    }

    compile_exporting_module(specifier, transpiled.as_str())
}

fn compile_legacy_expression_module(
    specifier: &str,
    expression_source: &str,
) -> RuntimeResult<String> {
    let expression = expression_source.trim().trim_end_matches(';');
    let statements = vec![format!("const __albedo_default_export__ = ({expression});")];
    let exports = vec!["__albedo_exports.default = __albedo_default_export__;".to_string()];
    build_module_record_script(specifier, &statements, &exports)
}

fn compile_exporting_module(specifier: &str, source: &str) -> RuntimeResult<String> {
    let lowered = lower_module_to_statements(specifier, source, &rewrite_import_declaration)?;
    build_module_record_script(specifier, &lowered.statements, &lowered.export_assignments)
}

/// One module body lowered to a flat list of classic-JS statements plus the
/// export assignments that publish its bindings (and which local, if any, holds
/// the default export). Shared by the server record wrapper
/// ([`compile_exporting_module`]) and the Tier-C client island builder
/// ([`compile_client_island_module`]) — the two differ only in the import
/// policy they pass in and how they wrap the result.
struct LoweredModule {
    statements: Vec<String>,
    export_assignments: Vec<String>,
    default_export_local: Option<String>,
}

/// A borrowed closure rather than a bare `fn` pointer, so a rewriter can CAPTURE
/// state — specifically the project's module sources, which is what lets the
/// client-island rewriter inline an imported data module instead of refusing it.
type ImportRewriter<'a> = &'a dyn Fn(swc_ecma_ast::ImportDecl, &str) -> RuntimeResult<Vec<String>>;

fn lower_module_to_statements(
    specifier: &str,
    source: &str,
    rewrite_import: ImportRewriter<'_>,
) -> RuntimeResult<LoweredModule> {
    let module = parse_module(specifier, source)?;
    let mut statements = Vec::new();
    let mut export_assignments = Vec::new();
    let mut default_export_local: Option<String> = None;

    for item in module.body {
        match item {
            ModuleItem::Stmt(stmt) => {
                let snippet = normalize_statement(slice_source(source, stmt.span(), specifier)?);
                if !snippet.is_empty() {
                    statements.push(snippet);
                }
            }
            ModuleItem::ModuleDecl(decl) => match decl {
                ModuleDecl::ExportDefaultExpr(default_expr) => {
                    let expr_source = slice_source(source, default_expr.expr.span(), specifier)?;
                    statements.push(format!(
                        "const __albedo_default_export__ = ({expr_source});"
                    ));
                    export_assignments
                        .push("__albedo_exports.default = __albedo_default_export__;".to_string());
                    default_export_local = Some("__albedo_default_export__".to_string());
                }
                ModuleDecl::ExportDefaultDecl(default_decl) => {
                    let decl_source = slice_source(source, default_decl.span(), specifier)?;
                    let default_value =
                        strip_export_default_prefix(&decl_source).ok_or_else(|| {
                            RuntimeError::load(
                                LoadErrorKind::UnsupportedSyntax,
                                format!(
                                "unsupported default export declaration in module '{specifier}'"
                            ),
                            )
                        })?;
                    statements.push(format!(
                        "const __albedo_default_export__ = {default_value};"
                    ));
                    export_assignments
                        .push("__albedo_exports.default = __albedo_default_export__;".to_string());
                    default_export_local = Some("__albedo_default_export__".to_string());
                }
                ModuleDecl::ExportDecl(export_decl) => match export_decl.decl {
                    Decl::Fn(fn_decl) => {
                        let decl_source = normalize_statement(slice_source(
                            source,
                            fn_decl.function.span,
                            specifier,
                        )?);
                        if !decl_source.is_empty() {
                            statements.push(decl_source);
                        }
                        let export_name = fn_decl.ident.sym.to_string();
                        let export_key = js_string_literal(&export_name, specifier)?;
                        export_assignments
                            .push(format!("__albedo_exports[{export_key}] = {export_name};"));
                    }
                    Decl::Var(var_decl) => {
                        let decl_source =
                            normalize_statement(slice_source(source, var_decl.span, specifier)?);
                        if !decl_source.is_empty() {
                            statements.push(decl_source);
                        }

                        for decl in var_decl.decls {
                            let export_name = match decl.name {
                                Pat::Ident(binding_ident) => binding_ident.id.sym.to_string(),
                                _ => {
                                    return Err(RuntimeError::load(
                                        LoadErrorKind::UnsupportedSyntax,
                                        format!(
                                            "unsupported export pattern in module '{specifier}'; only identifier bindings are supported"
                                        ),
                                    ));
                                }
                            };
                            let export_key = js_string_literal(&export_name, specifier)?;
                            export_assignments
                                .push(format!("__albedo_exports[{export_key}] = {export_name};"));
                        }
                    }
                    Decl::Class(class_decl) => {
                        // Slice the full `export class X …` then drop the
                        // `export` prefix so the class declaration stays a
                        // hoistable statement inside the record.
                        let decl_source = slice_source(source, export_decl.span, specifier)?;
                        let stripped = decl_source
                            .trim_start()
                            .strip_prefix("export")
                            .map(str::trim_start)
                            .ok_or_else(|| {
                                RuntimeError::load(
                                    LoadErrorKind::UnsupportedSyntax,
                                    format!(
                                        "unsupported exported class declaration in module '{specifier}'"
                                    ),
                                )
                            })?;
                        statements.push(normalize_statement(stripped.to_string()));
                        let export_name = class_decl.ident.sym.to_string();
                        let export_key = js_string_literal(&export_name, specifier)?;
                        export_assignments
                            .push(format!("__albedo_exports[{export_key}] = {export_name};"));
                    }
                    other => {
                        return Err(RuntimeError::load(
                            LoadErrorKind::UnsupportedSyntax,
                            format!(
                                "unsupported export declaration '{:?}' in module '{specifier}'",
                                other
                            ),
                        ));
                    }
                },
                ModuleDecl::ExportNamed(named_export) => {
                    if named_export.src.is_some() {
                        return Err(RuntimeError::load(
                            LoadErrorKind::UnsupportedSyntax,
                            format!(
                                "re-export from external source is not supported in module '{specifier}'"
                            ),
                        ));
                    }

                    for named_specifier in named_export.specifiers {
                        match named_specifier {
                            ExportSpecifier::Named(named) => {
                                let local = module_export_name_to_ident(&named.orig).ok_or_else(|| {
                                    RuntimeError::load(
                                        LoadErrorKind::UnsupportedSyntax,
                                        format!(
                                            "unsupported named export source in module '{specifier}'"
                                        ),
                                    )
                                })?;
                                let exported = named
                                    .exported
                                    .as_ref()
                                    .and_then(module_export_name_to_ident)
                                    .unwrap_or_else(|| local.clone());

                                let export_key = js_string_literal(&exported, specifier)?;
                                export_assignments
                                    .push(format!("__albedo_exports[{export_key}] = {local};"));
                            }
                            ExportSpecifier::Default(default_export) => {
                                let local = default_export.exported.sym.to_string();
                                export_assignments
                                    .push(format!("__albedo_exports.default = {local};"));
                            }
                            ExportSpecifier::Namespace(_) => {
                                return Err(RuntimeError::load(
                                    LoadErrorKind::UnsupportedSyntax,
                                    format!(
                                        "namespace exports are not supported in module '{specifier}'"
                                    ),
                                ));
                            }
                        }
                    }
                }
                ModuleDecl::Import(import_decl) => {
                    let rewritten = rewrite_import(import_decl, specifier)?;
                    statements.extend(rewritten);
                }
                unsupported => {
                    return Err(RuntimeError::load(
                        LoadErrorKind::UnsupportedSyntax,
                        format!(
                            "unsupported module declaration '{:?}' in module '{specifier}'",
                            unsupported
                        ),
                    ));
                }
            },
        }
    }

    if default_export_local.is_none() {
        // `export { X as default }` form — recover the local that the export
        // assignment binds, so the client builder can register it.
        for assignment in &export_assignments {
            if let Some(rest) = assignment.strip_prefix("__albedo_exports.default = ") {
                default_export_local = Some(rest.trim_end_matches(';').to_string());
                break;
            }
        }
    }

    Ok(LoweredModule {
        statements,
        export_assignments,
        default_export_local,
    })
}

/// Resolve a relative import specifier against the module map.
///
/// Module keys are whatever the caller registered — absolute source paths in
/// the CLI's case — so this joins the importer's directory and then tries the
/// extensions a TypeScript project actually uses.
fn resolve_relative_module(
    importer: &str,
    source: &str,
    modules: &HashMap<String, String>,
) -> Option<String> {
    let dir = Path::new(importer).parent()?;
    let joined = dir.join(source);
    // Normalize `..` segments so `src/components/../config` becomes `src/config`.
    let mut normalized = PathBuf::new();
    for part in joined.components() {
        match part {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let base = normalized.to_string_lossy().replace('\\', "/");
    let candidates = [
        base.clone(),
        format!("{base}.ts"),
        format!("{base}.tsx"),
        format!("{base}.js"),
        format!("{base}.jsx"),
        format!("{base}/index.ts"),
        format!("{base}/index.tsx"),
    ];
    modules.keys().find(|key| {
        let normalized_key = key.replace('\\', "/");
        candidates.iter().any(|candidate| normalized_key == *candidate)
    }).cloned()
}

/// Where one name imported from an npm package lives in the client chunk.
///
/// Produced by `bundler::client_npm` and consumed by the island lowering, which
/// is the whole reason it is a type and not two `String`s: the lowering must not
/// be able to invent a record key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientNpmBinding {
    /// The record key `__albedo_require_record` resolves.
    pub record_key: String,
    /// The property on that record, `default` for a default export, or `*` when
    /// the importer needs the record itself.
    pub export_name: String,
}

/// One island's npm bindings: specifier → imported name → where it lives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientNpmBindings {
    entries: BTreeMap<String, BTreeMap<String, ClientNpmBinding>>,
}

impl ClientNpmBindings {
    /// Record where one imported name resolves.
    pub fn insert(&mut self, specifier: &str, name: &str, binding: ClientNpmBinding) {
        self.entries
            .entry(specifier.to_string())
            .or_default()
            .insert(name.to_string(), binding);
    }

    /// Look one name up.
    #[must_use]
    pub fn get(&self, specifier: &str, name: &str) -> Option<&ClientNpmBinding> {
        self.entries.get(specifier)?.get(name)
    }

    /// Any binding at all for this specifier — the signal that the package
    /// resolved, as opposed to failing and leaving the island to refuse.
    #[must_use]
    pub fn contains_specifier(&self, specifier: &str) -> bool {
        self.entries.contains_key(specifier)
    }

    /// `true` when this island imports no npm package.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Tier-C client island import policy.
///
/// Framework runtime imports bind to the globals the client runtime installs.
/// A **relative project import** is inlined: the target module is lowered and
/// embedded in the island's own bundle, so the island can read a shared data
/// module the same way a Tier-A component can.
///
/// ── WHY THIS EXISTS ──────────────────────────────────────────────────
/// This used to reject every non-framework import outright, and the rejection
/// was swallowed at both call sites — so an island that referenced ANY imported
/// binding, an array or even a bare string, was classified Tier C, reported as
/// "ships a client island", and then never compiled. It rendered as an empty
/// `<div data-albedo-tier="c">` with a green build.
///
/// The practical cost was that a shared `config.ts` could not be read from an
/// island at all. Data had to be duplicated into every island that used it,
/// which is the opposite of what a compiler that owns the whole graph should
/// require.
///
/// ── HOW THE INLINING IS SCOPED ───────────────────────────────────────
/// The target module is wrapped in its own IIFE and only the imported names are
/// lifted out:
///
/// ```js
/// const __albedo_import_0 = (function(){ …target statements…
///   return { nav: nav }; })();
/// const nav = __albedo_import_0.nav;
/// ```
///
/// The IIFE is not decoration. Inlining the target's statements directly into
/// the island scope would leak its PRIVATE top-level bindings — a `const
/// suffix` in the data module would collide with an unrelated `suffix` in the
/// island. Wrapping keeps the target's internals invisible and lifts exactly
/// the imported bindings, which also makes aliasing (`import { a as b }`) a
/// plain property read.
fn rewrite_import_for_client_with_modules(
    import_decl: swc_ecma_ast::ImportDecl,
    specifier: &str,
    modules: &HashMap<String, String>,
    npm: &ClientNpmBindings,
    depth: u32,
) -> RuntimeResult<Vec<String>> {
    // Import chains are inlined recursively; the cap stops a cycle from
    // recursing until the stack dies. The manifest optimizer rejects true
    // cycles earlier, so this is a backstop rather than the primary guard.
    const MAX_INLINE_DEPTH: u32 = 8;

    let import_source = import_decl.src.value.to_string();
    if is_framework_runtime_import(import_source.as_str()) {
        return rewrite_framework_runtime_import(import_decl, specifier);
    }

    if import_source.starts_with('.') && depth < MAX_INLINE_DEPTH {
        if let Some(target_key) = resolve_relative_module(specifier, &import_source, modules) {
            let target_source = modules
                .get(&target_key)
                .expect("resolve_relative_module returned a key from this map");
            let transpiled =
                transpile_module_source_for_quickjs(&target_key, target_source, None)?;
            let nested = |decl: swc_ecma_ast::ImportDecl, spec: &str| {
                rewrite_import_for_client_with_modules(decl, spec, modules, npm, depth + 1)
            };
            let lowered = lower_module_to_statements(&target_key, transpiled.as_str(), &nested)?;

            // Parse `export_assignments` back into the pairs the IIFE returns.
            //
            // ⚠️ TWO SHAPES, and missing one is silent. A named export is
            // emitted through `js_string_literal` as
            //   __albedo_exports["nav"] = nav;
            // while `default` takes the dot form
            //   __albedo_exports.default = __albedo_default_export__;
            // Parsing only the dot form left `returns` empty for every data
            // module, so the IIFE emitted `return {  };`, every imported
            // binding was `undefined`, and the island threw on first render —
            // which looks exactly like "islands still don't work" rather than
            // like a parsing bug two layers down.
            let mut returns = Vec::new();
            for assignment in &lowered.export_assignments {
                let Some(rest) = assignment.strip_prefix("__albedo_exports") else {
                    continue;
                };
                let Some((key_part, local)) = rest.split_once(" = ") else {
                    continue;
                };
                let key = key_part.trim();
                let name = if let Some(bracketed) = key.strip_prefix('[') {
                    // `["nav"]` → `nav`
                    bracketed
                        .trim_end_matches(']')
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_string()
                } else {
                    key.trim_start_matches('.').to_string()
                };
                if name.is_empty() {
                    continue;
                }
                returns.push((
                    name,
                    local.trim_end_matches(';').trim().to_string(),
                ));
            }
            if returns.is_empty() {
                return Err(RuntimeError::load(
                    LoadErrorKind::UnsupportedSyntax,
                    format!(
                        "Tier-C client island '{specifier}' imports '{import_source}', but no \
                         exports could be lifted from that module — the island would receive \
                         `undefined` for every imported binding"
                    ),
                ));
            }

            let slot = format!(
                "__albedo_import_{:x}",
                fnv1a_32(format!("{specifier}:{import_source}").as_bytes())
            );
            let body = lowered.statements.join("\n");
            let return_obj = returns
                .iter()
                .map(|(name, local)| format!("{name}: {local}"))
                .collect::<Vec<_>>()
                .join(", ");

            let mut out = vec![format!(
                "const {slot} = (function(){{\n{body}\nreturn {{ {return_obj} }};\n}})();"
            )];

            // Bind each local name the island actually imported.
            for spec in &import_decl.specifiers {
                match spec {
                    ImportSpecifier::Named(named) => {
                        let exported = match &named.imported {
                            Some(ModuleExportName::Ident(id)) => id.sym.to_string(),
                            Some(ModuleExportName::Str(s)) => s.value.to_string(),
                            None => named.local.sym.to_string(),
                        };
                        out.push(format!(
                            "const {} = {slot}.{exported};",
                            named.local.sym
                        ));
                    }
                    ImportSpecifier::Default(default_spec) => {
                        out.push(format!(
                            "const {} = {slot}.default;",
                            default_spec.local.sym
                        ));
                    }
                    ImportSpecifier::Namespace(ns) => {
                        out.push(format!("const {} = {slot};", ns.local.sym));
                    }
                }
            }
            return Ok(out);
        }
    }

    // Tier C · Phase 2 — a bare npm specifier binds to the client chunk.
    //
    // 🔑 **Nothing is inlined here.** The package lives in a content-hashed
    // `/_albedo/npm/<pkg>.<hash>.js` loaded before this script, and the island
    // reads it through the same lazy record linker the server uses. That is why
    // the depth cap above is irrelevant to package graphs and why two islands
    // importing the same package cost one transfer, not two.
    if npm.contains_specifier(import_source.as_str()) {
        return bind_npm_import_for_client(&import_decl, specifier, import_source.as_str(), npm);
    }

    // Tier C · Phase 3 — a Node built-in is named, not left to the generic
    // "did not resolve" sentence.
    //
    // 🔑 **The same table the bundler consults** (`runtime::node_builtins`), so
    // the two cannot drift. The bundler reaches it after `node_modules`
    // resolution fails; this is the *other* door — an island whose import never
    // produced a client binding — and it is the message a user sees first,
    // because the bundler's own reason lands in the build log.
    if let Some(reason) = crate::runtime::node_builtins::refusal(import_source.as_str()) {
        return Err(RuntimeError::load(
            LoadErrorKind::UnsupportedSyntax,
            format!("Tier-C client island '{specifier}': {reason}"),
        ));
    }

    Err(RuntimeError::load(
        LoadErrorKind::UnsupportedSyntax,
        format!(
            "Tier-C client island '{specifier}' imports '{import_source}', which did not resolve \
             for the browser; framework runtime imports (react/react-dom/albedo), relative \
             project modules and bundled npm packages resolve client-side. If this is an npm \
             package, the build log names why its client bundle failed."
        ),
    ))
}

/// Bind one npm import against the client chunk's record table.
///
/// Every specifier form lands on `__albedo_require_record(<key>)`, which is
/// **memoised and lazy**: the factory for a file runs at most once, on first
/// access, and the record is published before the body runs so an import cycle
/// inside the package observes a partially-initialized record rather than
/// recursing — Node's CommonJS discipline, unchanged from the server.
///
/// A demanded name that the bundler could not resolve is a **build error**, not
/// an `undefined` binding discovered when the island first renders. The bundler
/// already refuses at that point; this is the second gate, for the case where a
/// binding table and an import list disagree.
fn bind_npm_import_for_client(
    import_decl: &ImportDecl,
    specifier: &str,
    import_source: &str,
    npm: &ClientNpmBindings,
) -> RuntimeResult<Vec<String>> {
    let record_of = |name: &str| -> RuntimeResult<String> {
        let binding = npm.get(import_source, name).ok_or_else(|| {
            RuntimeError::load(
                LoadErrorKind::ModuleMissing,
                format!(
                    "Tier-C client island '{specifier}' imports '{name}' from '{import_source}', \
                     but the client bundle has no binding for it"
                ),
            )
        })?;
        let key = js_string_literal(&binding.record_key, specifier)?;
        let record = format!("globalThis.__albedo_require_record({key})");
        if binding.export_name == "*" {
            return Ok(record);
        }
        let property = js_string_literal(&binding.export_name, specifier)?;
        Ok(format!("{record}[{property}]"))
    };

    // `import "pkg"` — run the module for its effects. The bundler records this
    // as a `*` demand (the package is taken whole), so the record is the module.
    if import_decl.specifiers.is_empty() {
        return Ok(vec![format!("{};", record_of("*")?)]);
    }

    let mut out = Vec::with_capacity(import_decl.specifiers.len());
    for import_specifier in &import_decl.specifiers {
        match import_specifier {
            ImportSpecifier::Default(default_specifier) => {
                let expression = record_of("default")?;
                out.push(format!(
                    "const {} = {expression};",
                    default_specifier.local.sym
                ));
            }
            ImportSpecifier::Namespace(namespace_specifier) => {
                let expression = record_of("*")?;
                out.push(format!(
                    "const {} = {expression};",
                    namespace_specifier.local.sym
                ));
            }
            ImportSpecifier::Named(named) => {
                let imported = match &named.imported {
                    Some(ModuleExportName::Ident(ident)) => ident.sym.to_string(),
                    Some(ModuleExportName::Str(literal)) => literal.value.to_string(),
                    None => named.local.sym.to_string(),
                };
                let expression = record_of(&imported)?;
                out.push(format!("const {} = {expression};", named.local.sym));
            }
        }
    }
    Ok(out)
}

/// A3.2 — lower one Tier-C island component to a **browser** script.
///
/// The component is transpiled with the same JSX pragma as the server
/// ([`transpile_module_source_for_quickjs`]), then lowered to classic-JS
/// statements with framework imports bound to globals. The result is wrapped in
/// an IIFE that self-registers the default export with the client runtime under
/// `component_id`, so `__ALBEDO_HYDRATE_ISLAND` can resolve it. Bare `h`,
/// `useState`, … resolve to the globals `assets/albedo-client.js` installs — the
/// same mechanism that lets one transpiled module run on both sides.
pub fn compile_client_island_module(
    specifier: &str,
    source: &str,
    component_id: u64,
) -> RuntimeResult<String> {
    compile_client_island_module_with_modules(specifier, source, component_id, &HashMap::new())
}

/// [`compile_client_island_module`], with the project's module sources so a
/// relative import can be inlined into the island bundle.
///
/// Callers that hold the module map should prefer this: without it an island
/// referencing ANY imported binding fails to compile, and the caller's error
/// handling decides whether that is loud or silent. The map is keyed however
/// the caller registered its modules (absolute source paths, in the CLI's
/// case); `resolve_relative_module` normalizes separators before matching.
pub fn compile_client_island_module_with_modules(
    specifier: &str,
    source: &str,
    component_id: u64,
    modules: &HashMap<String, String>,
) -> RuntimeResult<String> {
    compile_client_island_module_with_npm(
        specifier,
        source,
        component_id,
        modules,
        &ClientNpmBindings::default(),
    )
}

/// [`compile_client_island_module_with_modules`], plus the island's npm bindings.
///
/// Tier C · Phase 2. `npm` comes from `bundler::client_npm::build_client_npm_graph`
/// and says, per bare specifier and per imported name, which record in the
/// content-hashed client chunk the binding resolves to. Without it a bare
/// specifier is refused, which is exactly the pre-Phase-2 behaviour — so every
/// caller that has no chunk pipeline keeps working unchanged, and the ones that
/// do get npm by passing one more argument.
pub fn compile_client_island_module_with_npm(
    specifier: &str,
    source: &str,
    component_id: u64,
    modules: &HashMap<String, String>,
    npm: &ClientNpmBindings,
) -> RuntimeResult<String> {
    let normalized = source.trim_start_matches('\u{feff}');
    // `None`: an island's markup is produced by the browser runtime, which has
    // no `__albedo_stable_id` — stamping here would be a ReferenceError on the
    // island's first render.
    let transpiled = transpile_module_source_for_quickjs(specifier, normalized, None)?;

    let lowered = if !transpiled.contains("export") && !transpiled.contains("import") {
        // A bare expression module (`(props) => …`) — the expression itself is
        // the default export.
        let expr = transpiled.trim().trim_end_matches(';');
        LoweredModule {
            statements: vec![format!("const __albedo_default_export__ = ({expr});")],
            export_assignments: Vec::new(),
            default_export_local: Some("__albedo_default_export__".to_string()),
        }
    } else {
        {
            let rewriter = |decl: swc_ecma_ast::ImportDecl, spec: &str| {
                rewrite_import_for_client_with_modules(decl, spec, modules, npm, 0)
            };
            lower_module_to_statements(specifier, transpiled.as_str(), &rewriter)?
        }
    };

    let default_local = lowered.default_export_local.ok_or_else(|| {
        RuntimeError::load(
            LoadErrorKind::UnsupportedSyntax,
            format!("Tier-C client island '{specifier}' has no default export to hydrate"),
        )
    })?;

    let id_literal = js_string_literal(&component_id.to_string(), specifier)?;
    let body = lowered.statements.join("\n");
    Ok(format!(
        "(function(){{\n{body}\nif(globalThis.__albedoClient){{globalThis.__albedoClient.registerComponent({id_literal}, {default_local});}}\n}})();\n"
    ))
}

/// How an npm file lowers to a record factory (decided by the resolver from
/// the file extension and the nearest `package.json` `"type"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NpmModuleFormat {
    /// An ES module — imports/exports are rewritten onto the record linker.
    Esm,
    /// A CommonJS module — wrapped with `module`/`exports`/`require` shims;
    /// the record gets `default = module.exports` plus copied named props
    /// (Node's CJS→ESM interop shape).
    Cjs,
    /// A JSON module — the parsed value is the `default` export, object keys
    /// are also exposed as named exports.
    Json,
}

/// A2 · lower one npm file to a **lazy factory registration script**.
///
/// Unlike project modules (eager records via [`compile_module_script_for_quickjs`]),
/// npm files register `__ALBEDO_NPM_FACTORIES[key] = function(__albedo_exports) {…}`
/// and only execute on first import — which is what makes load order
/// irrelevant and import cycles safe (see `build_npm_runtime_helpers_script`).
///
/// `resolve` maps every raw specifier appearing in `source` to its canonical
/// record key; a specifier missing from the map is a resolver bug and fails
/// loudly here rather than at run time.
pub(crate) fn compile_npm_module_script(
    key: &str,
    source: &str,
    format: NpmModuleFormat,
    resolve: &HashMap<String, String>,
) -> RuntimeResult<String> {
    let source = source.trim_start_matches('\u{feff}');
    match format {
        NpmModuleFormat::Esm => compile_npm_esm_module(key, source, resolve),
        NpmModuleFormat::Cjs => compile_npm_cjs_module(key, source, resolve),
        NpmModuleFormat::Json => compile_npm_json_module(key, source),
    }
}

fn npm_resolved_literal(
    resolve: &HashMap<String, String>,
    raw: &str,
    key: &str,
) -> RuntimeResult<String> {
    let resolved = resolve.get(raw).ok_or_else(|| {
        RuntimeError::load(
            LoadErrorKind::ModuleMissing,
            format!("npm module '{key}' references unresolved specifier '{raw}' (bundler bug)"),
        )
    })?;
    js_string_literal(resolved, key)
}

fn compile_npm_esm_module(
    key: &str,
    source: &str,
    resolve: &HashMap<String, String>,
) -> RuntimeResult<String> {
    let (module, _source_map) =
        parse_module_with_syntax(key, source, Syntax::Es(EsSyntax::default()))?;

    let mut statements = Vec::new();
    let mut export_assignments = Vec::new();

    for item in module.body {
        match item {
            ModuleItem::Stmt(stmt) => {
                let snippet = normalize_statement(slice_source(source, stmt.span(), key)?);
                if !snippet.is_empty() {
                    statements.push(snippet);
                }
            }
            ModuleItem::ModuleDecl(decl) => match decl {
                ModuleDecl::Import(import_decl) => {
                    let raw = import_decl.src.value.to_string();
                    let resolved_literal = npm_resolved_literal(resolve, &raw, key)?;
                    let record = format!("globalThis.__albedo_require_record({resolved_literal})");

                    if import_decl.specifiers.is_empty() {
                        statements.push(format!("{record};"));
                        continue;
                    }

                    let mut named_bindings = Vec::new();
                    for import_specifier in import_decl.specifiers {
                        match import_specifier {
                            ImportSpecifier::Default(default_specifier) => {
                                let local = default_specifier.local.sym.to_string();
                                statements.push(format!("const {local} = {record}.default;"));
                            }
                            ImportSpecifier::Namespace(namespace_specifier) => {
                                let local = namespace_specifier.local.sym.to_string();
                                statements.push(format!("const {local} = {record};"));
                            }
                            ImportSpecifier::Named(named_specifier) => {
                                let local = named_specifier.local.sym.to_string();
                                let binding = match named_specifier.imported.as_ref() {
                                    None => local.clone(),
                                    Some(ModuleExportName::Ident(imported_ident))
                                        if imported_ident.sym == named_specifier.local.sym =>
                                    {
                                        local.clone()
                                    }
                                    Some(imported_name) => {
                                        let property =
                                            module_export_name_to_property(imported_name, key)?;
                                        format!("{property}: {local}")
                                    }
                                };
                                named_bindings.push(binding);
                            }
                        }
                    }
                    if !named_bindings.is_empty() {
                        statements.push(format!(
                            "const {{ {} }} = {record};",
                            named_bindings.join(", ")
                        ));
                    }
                }
                ModuleDecl::ExportDefaultExpr(default_expr) => {
                    let expr_source = slice_source(source, default_expr.expr.span(), key)?;
                    statements.push(format!(
                        "const __albedo_default_export__ = ({expr_source});"
                    ));
                    export_assignments
                        .push("__albedo_exports.default = __albedo_default_export__;".to_string());
                }
                ModuleDecl::ExportDefaultDecl(default_decl) => {
                    let decl_source = slice_source(source, default_decl.span(), key)?;
                    let default_value =
                        strip_export_default_prefix(&decl_source).ok_or_else(|| {
                            RuntimeError::load(
                                LoadErrorKind::UnsupportedSyntax,
                                format!(
                                    "unsupported default export declaration in npm module '{key}'"
                                ),
                            )
                        })?;
                    statements.push(format!(
                        "const __albedo_default_export__ = {default_value};"
                    ));
                    export_assignments
                        .push("__albedo_exports.default = __albedo_default_export__;".to_string());
                }
                ModuleDecl::ExportDecl(export_decl) => {
                    // Slice the full `export <decl>` and drop the prefix so
                    // function/class declarations stay hoistable statements.
                    let decl_source = slice_source(source, export_decl.span, key)?;
                    let stripped = decl_source
                        .trim_start()
                        .strip_prefix("export")
                        .map(str::trim_start)
                        .ok_or_else(|| {
                            RuntimeError::load(
                                LoadErrorKind::UnsupportedSyntax,
                                format!("unsupported export declaration in npm module '{key}'"),
                            )
                        })?;
                    statements.push(normalize_statement(stripped.to_string()));

                    let mut export_names = Vec::new();
                    match export_decl.decl {
                        Decl::Fn(fn_decl) => export_names.push(fn_decl.ident.sym.to_string()),
                        Decl::Class(class_decl) => {
                            export_names.push(class_decl.ident.sym.to_string());
                        }
                        Decl::Var(var_decl) => {
                            for declarator in var_decl.decls {
                                match declarator.name {
                                    Pat::Ident(binding_ident) => {
                                        export_names.push(binding_ident.id.sym.to_string());
                                    }
                                    _ => {
                                        return Err(RuntimeError::load(
                                            LoadErrorKind::UnsupportedSyntax,
                                            format!(
                                                "unsupported export pattern in npm module '{key}'; only identifier bindings are supported"
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                        other => {
                            return Err(RuntimeError::load(
                                LoadErrorKind::UnsupportedSyntax,
                                format!(
                                    "unsupported export declaration '{other:?}' in npm module '{key}'"
                                ),
                            ));
                        }
                    }
                    for export_name in export_names {
                        let export_key = js_string_literal(&export_name, key)?;
                        export_assignments
                            .push(format!("__albedo_exports[{export_key}] = {export_name};"));
                    }
                }
                ModuleDecl::ExportNamed(named_export) => {
                    if let Some(src) = named_export.src.as_ref() {
                        // Re-export: `export { x as y } from "spec"` /
                        // `export * as ns from "spec"`.
                        let raw = src.value.to_string();
                        let resolved_literal = npm_resolved_literal(resolve, &raw, key)?;
                        let record =
                            format!("globalThis.__albedo_require_record({resolved_literal})");
                        for named_specifier in named_export.specifiers {
                            match named_specifier {
                                ExportSpecifier::Named(named) => {
                                    let orig_property =
                                        module_export_name_to_property(&named.orig, key)?;
                                    let orig_key = if orig_property.starts_with('"') {
                                        orig_property
                                    } else {
                                        js_string_literal(&orig_property, key)?
                                    };
                                    let exported = named
                                        .exported
                                        .as_ref()
                                        .map(|name| module_export_name_to_property(name, key))
                                        .transpose()?
                                        .unwrap_or_else(|| orig_key.trim_matches('"').to_string());
                                    let exported_key = if exported.starts_with('"') {
                                        exported
                                    } else {
                                        js_string_literal(&exported, key)?
                                    };
                                    export_assignments.push(format!(
                                        "__albedo_exports[{exported_key}] = {record}[{orig_key}];"
                                    ));
                                }
                                ExportSpecifier::Namespace(namespace) => {
                                    let exported =
                                        module_export_name_to_property(&namespace.name, key)?;
                                    let exported_key = if exported.starts_with('"') {
                                        exported
                                    } else {
                                        js_string_literal(&exported, key)?
                                    };
                                    export_assignments.push(format!(
                                        "__albedo_exports[{exported_key}] = {record};"
                                    ));
                                }
                                ExportSpecifier::Default(_) => {
                                    return Err(RuntimeError::load(
                                        LoadErrorKind::UnsupportedSyntax,
                                        format!(
                                            "unsupported default re-export form in npm module '{key}'"
                                        ),
                                    ));
                                }
                            }
                        }
                    } else {
                        for named_specifier in named_export.specifiers {
                            match named_specifier {
                                ExportSpecifier::Named(named) => {
                                    let local =
                                        module_export_name_to_ident(&named.orig).ok_or_else(|| {
                                            RuntimeError::load(
                                                LoadErrorKind::UnsupportedSyntax,
                                                format!(
                                                    "unsupported named export source in npm module '{key}'"
                                                ),
                                            )
                                        })?;
                                    let exported = named
                                        .exported
                                        .as_ref()
                                        .and_then(module_export_name_to_ident)
                                        .unwrap_or_else(|| local.clone());
                                    let export_key = js_string_literal(&exported, key)?;
                                    export_assignments
                                        .push(format!("__albedo_exports[{export_key}] = {local};"));
                                }
                                ExportSpecifier::Default(default_export) => {
                                    let local = default_export.exported.sym.to_string();
                                    export_assignments
                                        .push(format!("__albedo_exports.default = {local};"));
                                }
                                ExportSpecifier::Namespace(_) => {
                                    return Err(RuntimeError::load(
                                        LoadErrorKind::UnsupportedSyntax,
                                        format!(
                                            "namespace export without a source in npm module '{key}'"
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                }
                ModuleDecl::ExportAll(export_all) => {
                    // `export * from "spec"` — copy enumerable own props except
                    // `default`. The `in` guard keeps the first star's binding
                    // when two stars collide, while later non-star assignments
                    // (locals always run unguarded) still win — ESM precedence.
                    let raw = export_all.src.value.to_string();
                    let resolved_literal = npm_resolved_literal(resolve, &raw, key)?;
                    export_assignments.push(format!(
                        "(function(__albedo_star) {{ for (const __albedo_k in __albedo_star) {{ if (__albedo_k !== 'default' && !(__albedo_k in __albedo_exports)) {{ __albedo_exports[__albedo_k] = __albedo_star[__albedo_k]; }} }} }})(globalThis.__albedo_require_record({resolved_literal}));"
                    ));
                }
                unsupported => {
                    return Err(RuntimeError::load(
                        LoadErrorKind::UnsupportedSyntax,
                        format!(
                            "unsupported module declaration '{unsupported:?}' in npm module '{key}'"
                        ),
                    ));
                }
            },
        }
    }

    build_npm_factory_script(key, &statements, &export_assignments)
}

fn compile_npm_cjs_module(
    key: &str,
    source: &str,
    resolve: &HashMap<String, String>,
) -> RuntimeResult<String> {
    let key_literal = js_string_literal(key, key)?;
    let dir = key.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(key);
    let dir_literal = js_string_literal(dir, key)?;
    let map_literal = serde_json::to_string(resolve).map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!("failed to serialize require map for npm module '{key}': {err}"),
        )
    })?;

    let mut script = String::new();
    script.push_str(&format!(
        "globalThis.__ALBEDO_NPM_FACTORIES[{key_literal}] = function(__albedo_exports) {{\n"
    ));
    script.push_str("  const __albedo_module = { exports: {} };\n");
    script.push_str(&format!("  const __albedo_require_map = {map_literal};\n"));
    script.push_str(&format!(
        "  const __albedo_cjs_require = function(specifier) {{\n    const spec = String(specifier);\n    if (!Object.prototype.hasOwnProperty.call(__albedo_require_map, spec)) {{\n      throw new Error('{MODULE_MISSING_MARKER}' + spec);\n    }}\n    const record = globalThis.__albedo_require_record(__albedo_require_map[spec]);\n    return (record && record.__albedo_cjs === true) ? record.default : record;\n  }};\n"
    ));
    script.push_str(&format!(
        "  (function(module, exports, require, __filename, __dirname, global) {{\n{source}\n  }})(__albedo_module, __albedo_module.exports, __albedo_cjs_require, {key_literal}, {dir_literal}, globalThis);\n"
    ));
    script.push_str("  const __albedo_value = __albedo_module.exports;\n");
    script.push_str(
        "  Object.defineProperty(__albedo_exports, '__albedo_cjs', { value: true, enumerable: false });\n",
    );
    script.push_str("  __albedo_exports['default'] = __albedo_value;\n");
    script.push_str(
        "  if (__albedo_value && (typeof __albedo_value === 'object' || typeof __albedo_value === 'function')) {\n    for (const __albedo_k of Object.keys(__albedo_value)) {\n      if (__albedo_k !== 'default') { __albedo_exports[__albedo_k] = __albedo_value[__albedo_k]; }\n    }\n  }\n",
    );
    script.push_str("};");
    Ok(script)
}

fn compile_npm_json_module(key: &str, source: &str) -> RuntimeResult<String> {
    // Parse + re-serialize: validates the JSON and canonicalizes any
    // formatting quirks (BOM already stripped) into a safe JS literal.
    let value: serde_json::Value = serde_json::from_str(source).map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::UnsupportedSyntax,
            format!("invalid JSON in npm module '{key}': {err}"),
        )
    })?;
    let value_literal = serde_json::to_string(&value).map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!("failed to re-serialize JSON module '{key}': {err}"),
        )
    })?;
    let key_literal = js_string_literal(key, key)?;

    Ok(format!(
        "globalThis.__ALBEDO_NPM_FACTORIES[{key_literal}] = function(__albedo_exports) {{\n  const __albedo_value = ({value_literal});\n  __albedo_exports['default'] = __albedo_value;\n  if (__albedo_value && typeof __albedo_value === 'object' && !Array.isArray(__albedo_value)) {{\n    for (const __albedo_k of Object.keys(__albedo_value)) {{\n      if (__albedo_k !== 'default') {{ __albedo_exports[__albedo_k] = __albedo_value[__albedo_k]; }}\n    }}\n  }}\n}};"
    ))
}

fn build_npm_factory_script(
    key: &str,
    statements: &[String],
    export_assignments: &[String],
) -> RuntimeResult<String> {
    let key_literal = js_string_literal(key, key)?;
    let mut script = String::new();
    script.push_str(&format!(
        "globalThis.__ALBEDO_NPM_FACTORIES[{key_literal}] = function(__albedo_exports) {{\n"
    ));
    for statement in statements {
        if statement.trim().is_empty() {
            continue;
        }
        script.push_str("  ");
        script.push_str(statement);
        if !statement.ends_with('\n') {
            script.push('\n');
        }
    }
    for export in export_assignments {
        script.push_str("  ");
        script.push_str(export);
        if !export.ends_with('\n') {
            script.push('\n');
        }
    }
    script.push_str("};");
    Ok(script)
}

fn transpile_module_source_for_quickjs(
    specifier: &str,
    source: &str,
    stamp_module_spec: Option<&str>,
) -> RuntimeResult<String> {
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let preferred_syntax = syntax_for_specifier(specifier);
        let (mut module, source_map) =
            parse_module_with_fallback(specifier, source, preferred_syntax)?;

        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        module.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, false));
        module.visit_mut_with(&mut strip_type());

        // B2 · stamp `{sharedSlot.map(...)}` containers with
        // `data-albedo-list-slot="topic"` while the tree is still JSX, so the
        // client sink can register the list as a keyed-list anchor bound to the
        // broadcast topic (the seam a FORGE write's `SlotDelta` fans into).
        crate::transforms::shared_slot_lists::mark_shared_slot_lists(&mut module);

        // B4 · the scalar half of the same seam: stamp `<span>{sharedSlot}</span>`
        // with `data-albedo-slot="topic"` so the client can register a text
        // binding that holds the ELEMENT. Without it the SSR span carries no
        // `data-albedo-id`, no binding site exists, and a broadcast `SlotSet`
        // strands in `pendingSlotValues` — live scalar reads never painted.
        // Runs after the list pass so a list container is already stamped and
        // the scalar marker skips it.
        crate::transforms::shared_slot_lists::mark_shared_slot_scalars(&mut module);

        // PRISM · fold `useSharedSlot`'s topic argument to something this engine
        // can evaluate. The shim stringifies whatever it is handed, and an
        // `albedo/forge` collection has no runtime record to stringify — the
        // pure-Rust interpreter never evaluates the call at all, so without this
        // the two engines disagree. A string-literal topic is left untouched,
        // which is every app that exists today.
        crate::transforms::shared_slots::rewrite_shared_slot_topic_args(&mut module);

        // Stamp `data-albedo-id` so the markup this engine renders is
        // addressable by the opcode frame built against the same component.
        // Runs LAST of the JSX passes and before the JSX lowering: the marker
        // passes above key off element shape (`<span>{topic}</span>`), and an
        // extra attribute must not change what they recognise.
        //
        // `None` for a Tier-C client island — it hydrates in the browser, where
        // `__albedo_stable_id` does not exist. See `transforms::stable_ids`.
        if let Some(spec) = stamp_module_spec {
            crate::transforms::stable_ids::stamp_stable_ids(&mut module, spec);
        }

        let mut jsx_options = JsxOptions::default();
        jsx_options.runtime = Some(JsxRuntime::Classic);
        jsx_options.pragma = Some("h".to_string());
        jsx_options.pragma_frag = Some("h.Fragment".to_string());
        jsx_options.development = Some(false);
        module.visit_mut_with(&mut jsx(
            source_map.clone(),
            None::<SingleThreadedComments>,
            jsx_options,
            top_level_mark,
            unresolved_mark,
        ));

        emit_module_source(specifier, &module, source_map)
    })
}

fn syntax_for_specifier(specifier: &str) -> Syntax {
    match Path::new(specifier)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("ts") => Syntax::Typescript(TsSyntax {
            tsx: false,
            decorators: true,
            ..Default::default()
        }),
        Some("tsx") => Syntax::Typescript(TsSyntax {
            tsx: true,
            decorators: true,
            ..Default::default()
        }),
        _ => Syntax::Es(EsSyntax {
            jsx: true,
            decorators: true,
            ..Default::default()
        }),
    }
}

fn parse_module_with_fallback(
    specifier: &str,
    source: &str,
    preferred_syntax: Syntax,
) -> RuntimeResult<(Module, Lrc<SourceMap>)> {
    let should_try_ts_fallback =
        matches!(preferred_syntax, Syntax::Es(_)) && Path::new(specifier).extension().is_none();

    match parse_module_with_syntax(specifier, source, preferred_syntax) {
        Ok(module) => Ok(module),
        Err(primary_error) => {
            if !should_try_ts_fallback {
                return Err(primary_error);
            }

            parse_module_with_syntax(
                specifier,
                source,
                Syntax::Typescript(TsSyntax {
                    tsx: true,
                    decorators: true,
                    ..Default::default()
                }),
            )
            .map_err(|_| primary_error)
        }
    }
}

fn parse_module_with_syntax(
    specifier: &str,
    source: &str,
    syntax: Syntax,
) -> RuntimeResult<(Module, Lrc<SourceMap>)> {
    let source_map: Lrc<SourceMap> = Default::default();
    let source_file = source_map.new_source_file(
        FileName::Custom(format!("quickjs:{specifier}")).into(),
        source.to_string(),
    );

    let mut parser = Parser::new(syntax, StringInput::from(&*source_file), None);
    parser
        .parse_module()
        .map(|module| (module, source_map))
        .map_err(|err| {
            RuntimeError::load(
                LoadErrorKind::UnsupportedSyntax,
                format!("failed to parse module '{specifier}': {:?}", err),
            )
        })
}

fn parse_module(specifier: &str, source: &str) -> RuntimeResult<Module> {
    let source_map: Rc<SourceMap> = Rc::new(SourceMap::default());
    let source_file = source_map.new_source_file(
        FileName::Custom(format!("quickjs:{specifier}")).into(),
        source.to_string(),
    );

    let mut parser = Parser::new(
        Syntax::Es(EsSyntax {
            jsx: true,
            decorators: true,
            ..Default::default()
        }),
        StringInput::from(&*source_file),
        None,
    );

    parser.parse_module().map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::UnsupportedSyntax,
            format!("failed to parse module '{specifier}': {:?}", err),
        )
    })
}

fn emit_module_source(
    specifier: &str,
    module: &Module,
    source_map: Lrc<SourceMap>,
) -> RuntimeResult<String> {
    let mut output = Vec::new();
    {
        let mut emitter = Emitter {
            cfg: CodegenConfig::default(),
            comments: None,
            cm: source_map.clone(),
            wr: JsWriter::new(source_map, "\n", &mut output, None),
        };
        emitter.emit_module(module).map_err(|err| {
            RuntimeError::load(
                LoadErrorKind::EngineFailure,
                format!("failed to emit transpiled module '{specifier}': {err}"),
            )
        })?;
    }
    String::from_utf8(output).map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!("failed to decode transpiled module '{specifier}' as UTF-8: {err}"),
        )
    })
}

fn build_module_record_script(
    specifier: &str,
    statements: &[String],
    export_assignments: &[String],
) -> RuntimeResult<String> {
    let escaped_specifier = js_string_literal(specifier, specifier)?;

    let mut script = String::new();
    script.push_str("(function() {\n");
    script.push_str("  const __albedo_exports = Object.create(null);\n");
    script.push_str(&format!(
        "  Object.defineProperty(__albedo_exports, \"{MODULE_RECORD_FLAG}\", {{ value: true, enumerable: false }});\n"
    ));

    for statement in statements {
        if statement.trim().is_empty() {
            continue;
        }
        script.push_str("  ");
        script.push_str(statement);
        if !statement.ends_with('\n') {
            script.push('\n');
        }
    }

    for export in export_assignments {
        script.push_str("  ");
        script.push_str(export);
        if !export.ends_with('\n') {
            script.push('\n');
        }
    }

    script.push_str(&format!(
        "  globalThis.__ALBEDO_MODULES[{escaped_specifier}] = __albedo_exports;\n"
    ));
    script.push_str("})();");
    Ok(script)
}

fn js_string_literal(value: &str, specifier: &str) -> RuntimeResult<String> {
    serde_json::to_string(value).map_err(|err| {
        RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!(
                "failed to serialize JavaScript string literal for module '{specifier}': {err}"
            ),
        )
    })
}

fn slice_source(source: &str, span: Span, specifier: &str) -> RuntimeResult<String> {
    let start = span.lo.0.saturating_sub(1) as usize;
    let end = span.hi.0.saturating_sub(1) as usize;

    if end < start {
        return Err(RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!(
                "invalid span while transforming module '{specifier}' (start={start}, end={end})"
            ),
        ));
    }

    source.get(start..end).map(|slice| slice.to_string()).ok_or_else(|| {
        RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!(
                "span out of bounds while transforming module '{specifier}' (start={start}, end={end}, len={})",
                source.len()
            ),
        )
    })
}

fn normalize_statement(source: String) -> String {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.ends_with(';') || trimmed.ends_with('}') {
        trimmed.to_string()
    } else {
        format!("{trimmed};")
    }
}

fn strip_export_default_prefix(source: &str) -> Option<String> {
    let trimmed = source.trim_start();
    trimmed
        .strip_prefix("export default")
        .map(|rest| rest.trim().trim_end_matches(';').to_string())
}

/// The framework's own runtime modules. Their exports (`useState`,
/// `useSharedSlot`, `action`, …) have no loadable module record — they resolve
/// to the global shims installed by [`build_builtin_runtime_helpers_script`].
/// A1 · host-object bridge: routing these imports through `__albedo_require`
/// throws `MODULE_MISSING` at load, which is exactly why a real TSX hook
/// component could not render under QuickJS before. Binding them to globals
/// instead is what makes `import { useState } from "react"` load and run.
fn is_framework_runtime_import(source: &str) -> bool {
    matches!(source, "react" | "react-dom" | "albedo")
        || source == FORGE_BINDINGS_MODULE
        || source == SOURCE_BINDINGS_MODULE
}

/// The compiler-generated collection bindings (`import { messages } from
/// "albedo/forge"`). A **types-only** module: it exists as a generated `.d.ts`
/// for the editor and has no runtime record at all, because
/// `transforms::shared_slots::rewrite_shared_slot_topic_args` folds every use
/// away before this point.
///
/// The binding emitted below is therefore a tripwire, not a value — see
/// `forge_collection_stub`.
const FORGE_BINDINGS_MODULE: &str = "albedo/forge";

use crate::transforms::shared_slots::SOURCE_BINDINGS_MODULE;

/// What an `albedo/forge` name binds to when it survives the transpile fold.
///
/// It should never be reached. If it is, the fold missed a call shape and the
/// alternative is a bare `ReferenceError: messages is not defined` pointing at
/// the wrong layer — so this throws with the actual cause and the actual fix.
fn forge_collection_stub(collection: &str) -> String {
    let message = format!(
        "collection '{collection}' can only be read through useSharedSlot(...) — e.g. \
         useSharedSlot({collection}) or useSharedSlot({collection}.where({{ col: params.id }})). \
         It has no runtime value of its own."
    );
    let escaped = message.replace('\\', "\\\\").replace('\'', "\\'");
    format!("{{ where: function() {{ throw new Error('{escaped}'); }} }}")
}

/// What an `albedo/sources` name binds to when it survives the transpile fold.
///
/// [`SOURCE_BINDINGS_MODULE`] is APERTURE's exact analogue of
/// [`FORGE_BINDINGS_MODULE`] and has to be handled here for the same reason:
/// both are types-only modules with no loadable record, so routing the import
/// through `__albedo_require` throws `MODULE_MISSING` at **load** time and the
/// whole component fails before a line of it runs.
///
/// A `Proxy` rather than [`forge_collection_stub`]'s fixed `{ where }` object,
/// because the method being called is the author's **route name** — `status`,
/// `repo`, anything — and there is no fixed set to enumerate. Without the trap a
/// missed fold would surface as `undefined is not a function`, which points at
/// the call site instead of at the fold that should have removed it.
fn source_route_stub(source: &str) -> String {
    let message = format!(
        "source '{source}' can only be read through useSharedSlot(...) — e.g. \
         useSharedSlot({source}.someRoute({{ id: params.id }})). It has no runtime value of \
         its own."
    );
    let escaped = message.replace('\\', "\\\\").replace('\'', "\\'");
    format!(
        "new Proxy({{}}, {{ get: function() {{ \
         return function() {{ throw new Error('{escaped}'); }}; }} }})"
    )
}

fn rewrite_import_declaration(
    import_decl: swc_ecma_ast::ImportDecl,
    specifier: &str,
) -> RuntimeResult<Vec<String>> {
    let import_source = import_decl.src.value.to_string();

    if is_framework_runtime_import(import_source.as_str()) {
        return rewrite_framework_runtime_import(import_decl, specifier);
    }

    // A project-relative specifier (`./x`, `../x`) is registered under the
    // importer-resolved absolute `module_path`, not its as-written form, so we
    // collapse it against the importer here and let `__albedo_resolve_project`
    // recover the registered key at link time. Bare specifiers (npm, already
    // branched on above for framework) pass through verbatim.
    let specifier_expr = if is_relative_specifier(import_source.as_str()) {
        let base = resolve_project_specifier_base(specifier, import_source.as_str());
        format!(
            "__albedo_resolve_project({})",
            js_string_literal(base.as_str(), specifier)?
        )
    } else {
        js_string_literal(import_source.as_str(), specifier)?
    };

    // Side-effect import: still trigger module initialization.
    if import_decl.specifiers.is_empty() {
        return Ok(vec![format!(
            "__albedo_import_namespace({specifier_expr});"
        )]);
    }

    // Each binding goes through a kind-specific helper so npm records get real
    // ESM semantics (default = the `default` property; named imports
    // destructure the record itself) while project modules keep the legacy
    // `__albedo_require` unwrapping unchanged. The underlying record lookup is
    // memoized, so repeated calls for one declaration are cheap.
    let mut statements = Vec::new();
    let mut named_bindings = Vec::new();

    for import_specifier in import_decl.specifiers {
        match import_specifier {
            ImportSpecifier::Default(default_specifier) => {
                let local = default_specifier.local.sym.to_string();
                statements.push(format!(
                    "const {local} = __albedo_import_default({specifier_expr});"
                ));
            }
            ImportSpecifier::Namespace(namespace_specifier) => {
                let local = namespace_specifier.local.sym.to_string();
                statements.push(format!(
                    "const {local} = __albedo_import_namespace({specifier_expr});"
                ));
            }
            ImportSpecifier::Named(named_specifier) => {
                let local = named_specifier.local.sym.to_string();
                let binding = match named_specifier.imported.as_ref() {
                    None => local.clone(),
                    Some(ModuleExportName::Ident(imported_ident))
                        if imported_ident.sym == named_specifier.local.sym =>
                    {
                        local.clone()
                    }
                    Some(imported_name) => {
                        let property = module_export_name_to_property(imported_name, specifier)?;
                        format!("{property}: {local}")
                    }
                };
                named_bindings.push(binding);
            }
        }
    }

    if !named_bindings.is_empty() {
        statements.push(format!(
            "const {{ {} }} = __albedo_import_named({specifier_expr});",
            named_bindings.join(", ")
        ));
    }

    Ok(statements)
}

/// A relative module specifier — `./x` or `../x`. These name project files and
/// must be resolved against the importer to find their registered module key;
/// every other shape (bare npm specifier, already-branched framework import) is
/// looked up verbatim.
fn is_relative_specifier(source: &str) -> bool {
    source.starts_with("./") || source.starts_with("../")
}

/// Collapse a relative import against the importing module's path into a stable,
/// extensionless, forward-slashed base key — the form `__albedo_resolve_project`
/// probes for extensions against `__ALBEDO_MODULES`. The drive/prefix and root
/// are preserved so the base matches the scanner's absolute `module_path`
/// exactly (e.g. importer `A:\proj\src\routes\index.tsx` + `../content/essays`
/// → `A:/proj/src/content/essays`). `.`/`..` segments collapse like
/// [`super::eval::component::normalize_specifier`], but unlike it the prefix is
/// kept so absolute keys round-trip.
fn resolve_project_specifier_base(importer: &str, source: &str) -> String {
    use std::path::{Component, Path};

    let parent = Path::new(importer).parent().unwrap_or_else(|| Path::new(""));
    let joined = parent.join(source);

    let mut parts: Vec<String> = Vec::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(segment) => parts.push(segment.to_string_lossy().to_string()),
            Component::Prefix(prefix) => {
                parts.push(prefix.as_os_str().to_string_lossy().to_string());
            }
            // The root separator is implicit in the joined `/` output.
            Component::RootDir => {}
        }
    }
    parts.join("/")
}

/// Bind the names imported from a framework runtime module to the global
/// shims rather than a `__albedo_require` lookup. Named imports map to
/// `globalThis.<orig>`; a default/namespace import maps to a small object
/// exposing the hook surface (so `React.useState` / `React.Fragment` still
/// resolve). A string-literal named import (`import { "x" as y }`) has no
/// global identifier to bind to and resolves to `undefined`.
fn rewrite_framework_runtime_import(
    import_decl: swc_ecma_ast::ImportDecl,
    _specifier: &str,
) -> RuntimeResult<Vec<String>> {
    // The shape `React`/namespace imports get bound to. Mirrors the hook
    // globals installed at engine init.
    const FRAMEWORK_NAMESPACE_OBJECT: &str = "{ useState: globalThis.useState, \
useSharedSlot: globalThis.useSharedSlot, useEffect: globalThis.useEffect, \
useLayoutEffect: globalThis.useLayoutEffect, useRef: globalThis.useRef, \
useMemo: globalThis.useMemo, useCallback: globalThis.useCallback, \
useContext: globalThis.useContext, createContext: globalThis.createContext, \
action: globalThis.action, Fragment: (globalThis.h && globalThis.h.Fragment) }";

    if import_decl.specifiers.is_empty() {
        // A bare side-effect import of a framework module is a no-op at load.
        return Ok(Vec::new());
    }

    // `albedo/forge` and `albedo/sources` names have no globals to bind to — the
    // transpile fold has already replaced every legitimate use. Bind a tripwire
    // so a shape the fold missed reports its own cause instead of a bare
    // ReferenceError.
    let bindings_stub: Option<fn(&str) -> String> = match import_decl.src.value.as_ref() {
        FORGE_BINDINGS_MODULE => Some(forge_collection_stub),
        SOURCE_BINDINGS_MODULE => Some(source_route_stub),
        _ => None,
    };
    if let Some(stub) = bindings_stub {
        let mut statements = Vec::new();
        for import_specifier in &import_decl.specifiers {
            let (local, collection) = match import_specifier {
                ImportSpecifier::Named(named) => {
                    let local = named.local.sym.to_string();
                    let exported = match named.imported.as_ref() {
                        None => local.clone(),
                        Some(ModuleExportName::Ident(ident)) => ident.sym.to_string(),
                        Some(ModuleExportName::Str(s)) => s.value.to_string(),
                    };
                    (local, exported)
                }
                ImportSpecifier::Default(spec) => {
                    let local = spec.local.sym.to_string();
                    (local.clone(), local)
                }
                ImportSpecifier::Namespace(spec) => {
                    let local = spec.local.sym.to_string();
                    (local.clone(), local)
                }
            };
            statements.push(format!("const {local} = {};", stub(&collection)));
        }
        return Ok(statements);
    }

    let mut statements = Vec::new();
    for import_specifier in import_decl.specifiers {
        match import_specifier {
            ImportSpecifier::Default(default_specifier) => {
                let local = default_specifier.local.sym.to_string();
                statements.push(format!("const {local} = {FRAMEWORK_NAMESPACE_OBJECT};"));
            }
            ImportSpecifier::Namespace(namespace_specifier) => {
                let local = namespace_specifier.local.sym.to_string();
                statements.push(format!("const {local} = {FRAMEWORK_NAMESPACE_OBJECT};"));
            }
            ImportSpecifier::Named(named_specifier) => {
                let local = named_specifier.local.sym.to_string();
                let orig = match named_specifier.imported.as_ref() {
                    None => Some(local.clone()),
                    Some(ModuleExportName::Ident(imported_ident)) => {
                        Some(imported_ident.sym.to_string())
                    }
                    // `import { "weird-name" as x }` — no global identifier.
                    Some(ModuleExportName::Str(_)) => None,
                };
                match orig {
                    Some(orig) => {
                        statements.push(format!("const {local} = globalThis.{orig};"));
                    }
                    None => {
                        statements.push(format!("const {local} = undefined;"));
                    }
                }
            }
        }
    }

    Ok(statements)
}

fn module_export_name_to_property(
    name: &ModuleExportName,
    specifier: &str,
) -> RuntimeResult<String> {
    match name {
        ModuleExportName::Ident(ident) => Ok(ident.sym.to_string()),
        ModuleExportName::Str(string_literal) => {
            let value = string_literal.value.to_string();
            js_string_literal(value.as_str(), specifier)
        }
    }
}

fn module_export_name_to_ident(name: &ModuleExportName) -> Option<String> {
    match name {
        ModuleExportName::Ident(ident) => Some(ident.sym.to_string()),
        ModuleExportName::Str(_) => None,
    }
}

fn map_render_error(entry: &str, message: &str) -> RuntimeError {
    if let Some(specifier) = extract_marker_payload(message, MODULE_MISSING_MARKER) {
        return RuntimeError::load(
            LoadErrorKind::ModuleMissing,
            format!("module missing during render: '{specifier}'"),
        );
    }

    if let Some(entry_module) = extract_marker_payload(message, INVALID_ENTRY_EXPORT_MARKER) {
        return RuntimeError::load(
            LoadErrorKind::InvalidEntryExport,
            format!("invalid entry export for '{entry_module}': expected a default export"),
        );
    }

    // Keep the thrown text and the component path as separate fields. The
    // path belongs in developer diagnostics (logs / dev overlay, via Display)
    // but must never leak into a reader-facing `error.tsx` boundary — that
    // reads `thrown_message()`, which returns `message` alone.
    RuntimeError::RenderComponentError {
        component: entry.to_string(),
        message: message.to_string(),
    }
}

fn extract_marker_payload(message: &str, marker: &str) -> Option<String> {
    let index = message.find(marker)?;
    let tail = &message[(index + marker.len())..];

    let mut payload = String::new();
    for ch in tail.chars() {
        if ch.is_whitespace() || matches!(ch, '\n' | '\r' | '\'' | '"' | ')' | ']' | '}') {
            break;
        }
        payload.push(ch);
    }

    let value = payload.trim_matches(':').trim_matches(',').to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::compile_module_script_for_quickjs;

    #[test]
    fn test_compile_module_rewrites_import_declarations_to_runtime_requires() {
        let source = r#"
            import DefaultThing from "pkg/default";
            import { a, b as c } from "pkg/named";
            import * as ns from "pkg/ns";
            import "pkg/side-effect";

            export default function App() {
                return String(DefaultThing) + String(a) + String(c) + String(ns);
            }
        "#;

        let compiled = compile_module_script_for_quickjs("components/App.jsx", source).unwrap();
        assert!(
            compiled.contains(r#"const DefaultThing = __albedo_import_default("pkg/default");"#)
        );
        assert!(compiled.contains(r#"const { a, b: c } = __albedo_import_named("pkg/named");"#));
        assert!(compiled.contains(r#"const ns = __albedo_import_namespace("pkg/ns");"#));
        assert!(compiled.contains(r#"__albedo_import_namespace("pkg/side-effect");"#));
    }

    #[test]
    fn test_compile_module_transpiles_jsx_and_strips_typescript() {
        let source = r#"
            export default function App(props: { name: string }) {
                const title: string = props.name as string;
                return <main>{title}</main>;
            }
        "#;

        let compiled = compile_module_script_for_quickjs("components/App.tsx", source).unwrap();
        assert!(compiled.contains("h("));
        assert!(!compiled.contains("<main>"));
        assert!(!compiled.contains(": string"));
        assert!(!compiled.contains(" as string"));
    }

    /// APERTURE · `albedo/sources` is a runtime module, not something to load.
    ///
    /// The regression, found by the first browser run of a declared source: this
    /// import fell through to `__albedo_import_named("albedo/sources")`, which
    /// throws `MODULE_MISSING` at **load** time — so the component never ran at
    /// all and the page shipped a blank Tier-B stub. A1's whole test suite was
    /// green, because nothing in it ever loaded a component module.
    #[test]
    fn a_source_bindings_import_is_bound_rather_than_loaded() {
        let source = r#"
            import { useSharedSlot } from "albedo";
            import { ops } from "albedo/sources";
            export default function Ops() {
                const status = useSharedSlot(ops.status());
                return <span>{status}</span>;
            }
        "#;
        let compiled = compile_module_script_for_quickjs("routes/ops.tsx", source).unwrap();
        assert!(
            !compiled.contains("albedo/sources"),
            "the specifier must never reach a module lookup; got: {compiled}"
        );
        assert!(
            compiled.contains("data-albedo-slot") && compiled.contains("__albedo_topic"),
            "and the read must still be anchored for live paint; got: {compiled}"
        );
    }

    /// The tripwire, for a call shape the fold missed: a `Proxy` rather than a
    /// fixed object, because the method is the author's route name and there is
    /// no set to enumerate. It must name its own cause, not `undefined is not a
    /// function`.
    #[test]
    fn an_unfolded_source_reference_throws_with_its_own_cause() {
        let source = r#"
            import { ops } from "albedo/sources";
            export default function Ops() {
                return <span>{ops.status()}</span>;
            }
        "#;
        let compiled = compile_module_script_for_quickjs("routes/ops.tsx", source).unwrap();
        assert!(compiled.contains("new Proxy"), "got: {compiled}");
        assert!(
            compiled.contains("can only be read through useSharedSlot"),
            "got: {compiled}"
        );
    }

    /// B2 · the container of a `{sharedSlot.map(...)}` is stamped with
    /// `data-albedo-list-slot="topic"` so the client can register it as a keyed
    /// list anchor bound to the broadcast topic.
    #[test]
    fn shared_slot_list_container_is_stamped_with_list_slot_attr() {
        let source = r#"
            import { useSharedSlot } from "albedo";
            export default function Guestbook() {
                const entries = useSharedSlot("guestbook");
                return (
                    <ul className="entries">
                        {entries.map((entry) => (
                            <li key={entry.id}>{entry.author}</li>
                        ))}
                    </ul>
                );
            }
        "#;
        let compiled = compile_module_script_for_quickjs("routes/index.tsx", source).unwrap();
        assert!(
            compiled.contains("data-albedo-list-slot") && compiled.contains("guestbook"),
            "the shared-slot list container must carry data-albedo-list-slot=\"guestbook\"; got: {compiled}"
        );
    }

    /// A `.map()` over a plain local array is NOT a shared-slot list, so its
    /// container must not be stamped — the anchor is only for topic-fed lists.
    #[test]
    fn non_shared_slot_list_is_not_stamped() {
        let source = r#"
            export default function List() {
                const items = [{ id: 1 }, { id: 2 }];
                return <ul>{items.map((item) => <li key={item.id}>{item.id}</li>)}</ul>;
            }
        "#;
        let compiled = compile_module_script_for_quickjs("routes/list.tsx", source).unwrap();
        assert!(
            !compiled.contains("data-albedo-list-slot"),
            "a plain local list must not be stamped; got: {compiled}"
        );
    }

    #[test]
    fn test_prewarm_initializes_engine() {
        use super::QuickJsEngine;

        let engine = QuickJsEngine::new();
        assert!(!engine.is_initialized());

        let mut engine = engine;
        engine.prewarm();
        assert!(engine.is_initialized());
    }

    #[test]
    fn test_prewarm_is_idempotent() {
        use super::QuickJsEngine;

        let engine = QuickJsEngine::new();
        let mut engine = engine;

        engine.prewarm();
        assert!(engine.is_initialized());

        engine.prewarm();
        assert!(engine.is_initialized());
    }

    // A logic-heavy component: a loop, an array, an object with string keys, dynamic
    // attribute values — enough to make QuickJS intern atoms and allocate shapes per
    // render, which is exactly what the request reset has to survive.
    const STRESS_COMPONENT: &str = r#"
        export default function App(props) {
            const rows = [];
            for (let i = 0; i < props.n; i++) {
                rows.push(h('li', { 'data-idx': i }, 'row ' + i));
            }
            const meta = { title: props.title, count: rows.length };
            return h('ul', { id: meta.title, 'data-count': meta.count }, rows);
        }
    "#;

    // Movement III guardrail (Workstream V): every render bump-allocates into the request
    // region and the boundary reset returns it to empty, so steady-state renders add zero
    // persistent heap traffic. Re-rendering the same input across many resets must also
    // keep producing byte-identical, correct output — the corruption check for resetting
    // a shared runtime's arena out from under its global atom/shape tables.
    #[test]
    fn request_arena_resets_each_render_without_persistent_growth_or_corruption() {
        use super::{QuickJsEngine, ARENA_WARMUP_RENDERS};
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");
        engine
            .load_module("routes/stress", STRESS_COMPONENT)
            .expect("module load");

        let props = r#"{"n":6,"title":"grid"}"#;
        let expected = "<ul id=\"grid\" data-count=\"6\">\
<li data-idx=\"0\">row 0</li><li data-idx=\"1\">row 1</li>\
<li data-idx=\"2\">row 2</li><li data-idx=\"3\">row 3</li>\
<li data-idx=\"4\">row 4</li><li data-idx=\"5\">row 5</li></ul>";

        // Warm past ARENA_WARMUP_RENDERS so every render in the steady loop below is
        // request-scoped (begin_request + reset) and the persistent tables have settled.
        const WARMUP: u32 = ARENA_WARMUP_RENDERS + 2;
        const STEADY: usize = 200;

        for _ in 0..WARMUP {
            let out = engine
                .render_component("routes/stress", props)
                .expect("warmup render");
            assert_eq!(out.html, expected);
        }

        // Settle one post-warmup render, then snapshot the steady-state baseline.
        let _ = engine
            .render_component("routes/stress", props)
            .expect("settle render");
        let base = engine.arena_stats();
        assert!(
            base.persistent_used > 0,
            "warmup should have populated the persistent region"
        );
        assert!(
            base.system_peak_bytes > 0,
            "request memory should be served from the system allocator"
        );

        for i in 0..STEADY {
            let out = engine
                .render_component("routes/stress", props)
                .expect("steady render");
            // Correctness across requests: byte-identical output every time.
            assert_eq!(out.html, expected, "render {i} diverged");

            let stats = engine.arena_stats();
            // Zero per-tick persistent growth in steady state — warmup state is hot and
            // frozen; nothing a render allocates lands in the persistent region.
            assert_eq!(
                stats.persistent_used, base.persistent_used,
                "persistent region grew on steady-state render {i}"
            );
            // QuickJS frees each render's request memory (refcount + cycle collector), so
            // outstanding request bytes never ratchet up — no leak, no reset needed.
            assert_eq!(
                stats.system_live_bytes, base.system_live_bytes,
                "request memory leaked on steady-state render {i}"
            );
        }

        // Persistent capacity was never exceeded (no warmup-state spill to the system path).
        assert_eq!(
            engine.arena_stats().fallback_allocs,
            0,
            "persistent capacity was exceeded"
        );
    }

    // A dynamic `[slug]` route shape: a module-level dataset + a component that
    // reaches into `props.params.slug`. With empty props (`{}`) the lookup
    // dereferences `undefined` and THROWS — exactly what the boot warmup does for
    // a dynamic route, so its render-path shapes never intern persistently.
    const SLUG_COMPONENT: &str = r#"
        const ALL = [
            { slug: "alpha", title: "Alpha", body: ["one", "two"] },
            { slug: "beta", title: "Beta", body: ["three"] }
        ];
        export default function Essay(props) {
            const essay = ALL.find(function (e) { return e.slug === props.params.slug; });
            if (!essay) { throw new Error("no piece: " + props.params.slug); }
            return h('article', { 'data-slug': essay.slug }, [
                h('h1', {}, essay.title),
                h('div', { 'class': 'body' }, essay.body.map(function (p) { return h('p', {}, p); }))
            ]);
        }
    "#;

    // Repro for the dynamic-route arena residual hazard: when the warmup render
    // throws (props `{}` → no `params`), the route's render-path shapes are
    // created for the first time during a *scoped* request, interned into
    // QuickJS's global shape hash, then freed by the request reset — so the next
    // request reuses a dangling shape and aborts (`sh->header.ref_count == 0`).
    // The fix must keep these renders correct across many resets.
    #[test]
    fn dynamic_route_render_survives_reset_after_throwing_warmup() {
        use super::{QuickJsEngine, ARENA_WARMUP_RENDERS};
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");
        engine
            .load_module("routes/slug", SLUG_COMPONENT)
            .expect("module load");

        // Mirror the boot warmup verbatim: render with empty props (the dynamic
        // route throws — its render-path shapes never intern persistently).
        for _ in 0..(ARENA_WARMUP_RENDERS + 2) {
            let _ = engine.render_component("routes/slug", "{}");
        }

        // Steady scoped renders with real params. Pre-fix, the 2nd aborts.
        for i in 0..16 {
            let out = engine
                .render_component("routes/slug", r#"{"params":{"slug":"alpha"}}"#)
                .expect("scoped render");
            assert!(
                out.html.contains("data-slug=\"alpha\"") && out.html.contains("<p>one</p>"),
                "render {i} diverged: {}",
                out.html
            );
        }
    }

    // ── A1 · host-object bridge — handlers under QuickJS ──────────────────

    // A handler body using a `for` loop and `try`/`catch` — exactly the
    // constructs the pure-Rust evaluator rejects. Running it through QuickJS
    // proves the promotion: the body computes a sum in a loop, swallows an
    // error in a try, and lowers both a setter call and a broadcast to effects.
    #[test]
    fn eval_handler_runs_real_js_and_collects_effects_in_order() {
        use super::QuickJsEngine;
        use crate::ir::opcode::SlotId;
        use crate::runtime::bridge::{HandlerEffect, HandlerInvocation};
        use crate::runtime::broadcast::broadcast_slot_id;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
        use serde_json::{Map, Value};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let mut env: Map<String, Value> = Map::new();
        env.insert("base".to_string(), Value::from(10));
        let setters = vec![("setTotal".to_string(), SlotId(9))];

        let body = r#"
            let total = base;
            for (let i = 1; i <= 3; i++) { total += i; }
            try { JSON.parse("not json"); } catch (e) { total += 100; }
            setTotal(total);
            broadcast("chat:room", "hi");
        "#;
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let invocation = HandlerInvocation {
            body,
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &setters,
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };

        let effects = engine
            .eval_handler("routes/counter", &invocation)
            .expect("handler runs")
            .effects;

        // 10 + (1+2+3) + 100 = 116
        assert_eq!(
            effects[0],
            HandlerEffect::SlotSet {
                slot_id: SlotId(9),
                value: b"116".to_vec()
            }
        );
        match &effects[1] {
            HandlerEffect::Broadcast {
                topic,
                slot_id,
                value,
            } => {
                assert_eq!(topic, "chat:room");
                assert_eq!(*slot_id, broadcast_slot_id("chat:room"));
                assert_eq!(value, b"\"hi\"");
            }
            other => panic!("expected broadcast effect, got {other:?}"),
        }
        assert_eq!(effects.len(), 2);
    }

    // A throw inside the handler must surface loudly, not vanish.
    #[test]
    fn eval_handler_surfaces_thrown_errors() {
        use super::QuickJsEngine;
        use crate::runtime::bridge::HandlerInvocation;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
        use serde_json::Map;

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let env = Map::new();
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let invocation = HandlerInvocation {
            body: "throw new Error('handler exploded')",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };

        let err = engine
            .eval_handler("routes/boom", &invocation)
            .expect_err("a throw must propagate");
        assert!(
            err.to_string().contains("handler exploded"),
            "error should carry the thrown message, got: {err}"
        );
    }

    /// **Capability probe, not a feature test.** A2's replay design rests on
    /// "the engine cannot suspend a body", and the alternative to replay —
    /// lowering an `async` body to a generator and driving it with `.next(v)`
    /// from Rust — rests on the opposite. Which of those is true is a fact about
    /// this engine, so it is asserted here rather than assumed in a document.
    ///
    /// Generators are ES2015 and QuickJS is ES2020-complete, but the load-bearing
    /// part is the second half: a value passed INTO `.next(v)` becomes the result
    /// of the paused `yield`, which is exactly the resume protocol a suspended
    /// outbound call needs.
    #[test]
    fn the_engine_drives_generators_and_resumes_them_with_a_value() {
        use super::QuickJsEngine;
        use crate::runtime::bridge::{HandlerEffect, HandlerInvocation};
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
        use crate::ir::opcode::SlotId;
        use serde_json::Map;

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        let env = Map::new();
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let setters = vec![("out".to_string(), SlotId(1))];
        let invocation = HandlerInvocation {
            // Pause, receive a value from the driver, use it, pause again.
            body: "function* flow(){ const a = yield 'ask-a'; const b = yield 'ask-' + a; \
                   return a + '/' + b; } \
                   const g = flow(); const first = g.next(); const second = g.next('A'); \
                   const done = g.next('B'); \
                   out([first.value, second.value, done.value, done.done].join('|'));",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &setters,
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };

        let outcome = engine
            .eval_handler("routes/gen", &invocation)
            .expect("generators run");
        let value = match &outcome.effects[0] {
            HandlerEffect::SlotSet { value, .. } => String::from_utf8(value.clone()).unwrap(),
            other => panic!("expected a slot write, got {other:?}"),
        };
        assert_eq!(
            value, "\"ask-a|ask-A|A/B|true\"",
            "the engine can pause a body, hand the pause point out, and resume it \
             with a value — the resume protocol suspend/replay was assumed to lack"
        );
    }

    // ── APERTURE A2 · suspend and replay ─────────────────────────────────

    /// Drive a body to completion the way an async caller does: run a pass,
    /// resolve whatever it asked for, append, run again. `answer` stands in for
    /// the HTTP layer and records what it was asked, so these tests assert
    /// **counts** — how many times a call was actually issued — rather than
    /// timing.
    fn drive(
        engine: &mut super::QuickJsEngine,
        body: &str,
        setters: &[(String, crate::ir::opcode::SlotId)],
        mut answer: impl FnMut(&crate::runtime::bridge::PendingRequest) -> crate::aperture::StepOutcome,
    ) -> (
        crate::runtime::bridge::HandlerOutcome,
        crate::aperture::Journal,
        usize,
    ) {
        use crate::aperture::{Journal, StepKind};
        use crate::runtime::bridge::{HandlerInvocation, HandlerRun};
        use serde_json::Map;

        let env = Map::new();
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let mut journal = Journal::new("w_test", "build-test");
        let mut passes = 0usize;
        loop {
            passes += 1;
            assert!(passes < 10, "runaway replay");
            let seeded = journal.to_script_value();
            let invocation = HandlerInvocation {
                body,
                is_block: true,
                env: &env,
                raw_bindings: &[],
                setters,
                event_json: None,
                broadcast_current: &bc,
                journal: Some(&seeded),
            };
            match engine
                .eval_handler_run("routes/flow", &invocation)
                .expect("pass runs")
            {
                HandlerRun::Completed(outcome) => return (outcome, journal, passes),
                HandlerRun::Suspended { pending, .. } => {
                    for request in &pending {
                        let outcome = answer(request);
                        journal
                            .append(request.step, StepKind::Fetch, &request.digest, outcome)
                            .expect("append");
                    }
                }
            }
        }
    }

    fn response(status: u16, body: &str) -> crate::aperture::StepOutcome {
        crate::aperture::StepOutcome::Completed(serde_json::json!({
            "status": status,
            "body": body,
            "headers": { "content-type": "application/json" },
        }))
    }

    /// The protocol, end to end: a body that calls out suspends with the
    /// request staged and **no effects**, and the same body run again against a
    /// journal carrying the answer completes and emits them.
    #[test]
    fn a_body_that_calls_out_suspends_then_completes_on_replay() {
        use super::QuickJsEngine;
        use crate::ir::opcode::SlotId;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        let setters = vec![("setName".to_string(), SlotId(3))];
        let mut issued: Vec<String> = Vec::new();
        let (outcome, journal, passes) = drive(
            &mut engine,
            "const res = fetch('https://api.test/user/7'); setName(res.json().name);",
            &setters,
            |request| {
                issued.push(format!("{} {}", request.method, request.url));
                response(200, r#"{"name":"ada"}"#)
            },
        );

        assert_eq!(passes, 2, "one pass to ask, one to finish");
        assert_eq!(issued, vec!["GET https://api.test/user/7"]);
        assert_eq!(journal.len(), 1);
        assert_eq!(
            outcome.effects,
            vec![crate::runtime::bridge::HandlerEffect::SlotSet {
                slot_id: SlotId(3),
                value: b"\"ada\"".to_vec(),
            }],
            "the effect carries the value the upstream returned"
        );
    }

    /// 🔴 **This test falsifies `APERTURE.md` § 5.4's second property.**
    ///
    /// The design says *"independent fetches parallelise for free — everything
    /// issued in one pass resolves concurrently, `Promise.all` semantics without
    /// Promises. Only dependent chains cost a replay each."* Three independent
    /// GETs, written as three lines, cost **four passes and three round trips**.
    ///
    /// The reason is structural and was visible in the protocol all along: a
    /// missed `fetch` *throws*. The first call unwinds the body before the
    /// second is ever evaluated, so a pass can only ever stage more than one
    /// request if something put them there before the body ran.
    ///
    /// So § 11 R1.3's hoisting is **not an optimisation** — it is the only
    /// mechanism by which batching happens at all, and until it lands every
    /// outbound call costs a round trip whether or not the calls are
    /// independent. This test asserts the real number so the claim cannot be
    /// quietly believed again; it flips to 2 passes when hoisting lands.
    #[test]
    fn independent_calls_cost_a_pass_each_until_the_compiler_hoists_them() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        let mut issued = 0usize;
        let (_outcome, journal, passes) = drive(
            &mut engine,
            "const a = fetch('https://api.test/a'); const b = fetch('https://api.test/b'); \
             const c = fetch('https://api.test/c'); a.json(); b.json(); c.json();",
            &[],
            |_request| {
                issued += 1;
                response(200, "{}")
            },
        );

        assert_eq!(
            passes, 4,
            "one pass per call plus the completing pass — NOT the 2 the design claims"
        );
        assert_eq!(issued, 3);
        assert_eq!(journal.len(), 3);
    }

    /// A dependent chain costs a pass each — the honest cost of replay (§ 11
    /// R1), and the thing the compiler's hoisting pass exists to avoid paying
    /// when the calls were never actually dependent.
    #[test]
    fn a_dependent_chain_costs_one_pass_per_link() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        let (_outcome, journal, passes) = drive(
            &mut engine,
            "const one = fetch('https://api.test/first').json(); \
             const two = fetch('https://api.test/' + one.next).json(); two.done;",
            &[],
            |request| {
                if request.url.ends_with("/first") {
                    response(200, r#"{"next":"second"}"#)
                } else {
                    response(200, r#"{"done":true}"#)
                }
            },
        );

        assert_eq!(passes, 3, "two dependent links, two round trips, three passes");
        assert_eq!(journal.len(), 2);
    }

    /// 🧪 **The runtime-substrate gating test.** `FLOOR.md` § 5.8 asks: *one
    /// body awaits a host function twice, sequentially — does it execute
    /// **once**, and do the effects still arrive as **one set** at the end?*
    ///
    /// The timing half is **no** under replay, by construction: three passes
    /// for two dependent links (`a_dependent_chain_costs_one_pass_per_link`).
    /// This test pins the half that decides whether a future async bridge is
    /// an improvement or a regression wearing a speedup — **the authority
    /// property**. § 5.8's trap clause is explicit that a bridge which passes
    /// the timing test and drops this is a failure, not partial success.
    ///
    /// The body issues a **durable** write between each pair of awaits, so an
    /// implementation that accumulated effects across passes would emit
    /// `a, a, b, a, b, c` — six writes, four of them duplicates, and the last
    /// three double-charged against F1's rules. The contract is `a, b, c`:
    /// exactly one copy of each, delivered once, after the last await.
    ///
    /// 🔑 **The oracle a bridge must reproduce is the effect block below, not
    /// the pass count.** Today the one-set property is free: `__albedo_effects`
    /// is a fresh `const` per script (`bridge.rs`) and a suspended pass returns
    /// before any instruction is built (`compiled.rs`), so a discarded pass
    /// discards its writes by construction. **A true async bridge has no
    /// passes to discard** — it must hold the accumulating array across the
    /// suspension and apply it exactly once at the end, which means the
    /// property stops being free and starts being something you build.
    ///
    /// ⚠️ When that lands, `passes` becomes 1 and *that* assertion flips. The
    /// three effect assertions must not.
    #[test]
    fn two_sequential_awaits_replay_the_body_but_emit_exactly_one_effect_set() {
        use super::QuickJsEngine;
        use crate::runtime::bridge::HandlerEffect;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        let mut issued: Vec<String> = Vec::new();
        let (outcome, journal, passes) = drive(
            &mut engine,
            "append('trail', { step: 'a' }); \
             const one = fetch('https://api.test/first').json(); \
             append('trail', { step: 'b', via: one.next }); \
             const two = fetch('https://api.test/' + one.next).json(); \
             append('trail', { step: 'c', done: two.done });",
            &[],
            |request| {
                issued.push(request.url.clone());
                if request.url.ends_with("/first") {
                    response(200, r#"{"next":"second"}"#)
                } else {
                    response(200, r#"{"done":true}"#)
                }
            },
        );

        // Timing half — recorded, not celebrated.
        assert_eq!(passes, 3, "the body runs three times, not once");

        // No double-send: each upstream call is issued exactly once even
        // though the body that issues it ran three times.
        assert_eq!(
            issued,
            vec!["https://api.test/first", "https://api.test/second"],
            "the journal answered the replayed calls instead of re-issuing them"
        );
        assert_eq!(journal.len(), 2);

        // 🔑 The authority property: ONE set, one copy of each write.
        let steps: Vec<String> = outcome
            .effects
            .iter()
            .map(|effect| match effect {
                HandlerEffect::ForgeAppend { collection, record } => {
                    assert_eq!(collection, "trail");
                    record
                        .get("step")
                        .and_then(|v| v.as_str())
                        .expect("every write carries its step")
                        .to_string()
                }
                other => panic!("expected a durable append, got {other:?}"),
            })
            .collect();

        assert_eq!(
            steps,
            vec!["a", "b", "c"],
            "writes from the two suspending passes were discarded with them; \
             an accumulating implementation would read a,a,b,a,b,c"
        );
    }

    /// § 11 R3 — **the sentinel cannot be swallowed.** A body that wraps its
    /// call in `try/catch` (or hands it to a bundled library that does) would
    /// otherwise eat the suspension and run on garbage. The flag the epilogue
    /// checks means the worst case is "suspend anyway", never "commit the
    /// effects of a body that never got its data".
    #[test]
    fn a_userland_catch_cannot_swallow_the_suspension() {
        use super::QuickJsEngine;
        use crate::ir::opcode::SlotId;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        let setters = vec![("setName".to_string(), SlotId(1))];
        let (outcome, journal, passes) = drive(
            &mut engine,
            "let name = 'fallback'; \
             try { name = fetch('https://api.test/u').json().name; } catch (e) { name = 'swallowed'; } \
             setName(name);",
            &setters,
            |_| response(200, r#"{"name":"ada"}"#),
        );

        assert_eq!(passes, 2);
        assert_eq!(journal.len(), 1);
        assert_eq!(
            outcome.effects,
            vec![crate::runtime::bridge::HandlerEffect::SlotSet {
                slot_id: SlotId(1),
                value: b"\"ada\"".to_vec(),
            }],
            "the catch ran on the suspending pass and was DISCARDED with it"
        );
    }

    /// A failed step replays as a throw the body can see and handle — the
    /// journal records failure as an outcome, not as an absence.
    #[test]
    fn a_failed_step_replays_as_a_throw_the_body_can_catch() {
        use super::QuickJsEngine;
        use crate::ir::opcode::SlotId;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        let setters = vec![("setName".to_string(), SlotId(1))];
        let (outcome, _journal, passes) = drive(
            &mut engine,
            "let name; try { name = fetch('https://api.test/u').json().name; } \
             catch (e) { name = 'unreachable: ' + e.message; } setName(name);",
            &setters,
            |_| crate::aperture::StepOutcome::Failed("upstream refused the connection".into()),
        );

        assert_eq!(passes, 2);
        let value = match &outcome.effects[0] {
            crate::runtime::bridge::HandlerEffect::SlotSet { value, .. } => {
                String::from_utf8(value.clone()).unwrap()
            }
            other => panic!("expected a slot write, got {other:?}"),
        };
        assert!(
            value.contains("upstream refused the connection"),
            "the body caught the recorded failure; got {value}"
        );
    }

    /// § 10 — a replay that asks for something *different* at the same step is
    /// loud. It cannot be answered from the journal without lying about which
    /// request was made, and the step index is an idempotency key, so a silent
    /// re-key would be a double-send waiting to happen.
    #[test]
    fn a_divergent_replay_fails_loudly_instead_of_re_keying() {
        use super::QuickJsEngine;
        use crate::aperture::{Journal, StepKind, StepOutcome};
        use crate::runtime::bridge::HandlerInvocation;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
        use serde_json::Map;

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        // A journal recorded against a DIFFERENT url than the body asks for.
        let mut journal = Journal::new("w", "b");
        journal
            .append(
                0,
                StepKind::Fetch,
                "deadbeef",
                StepOutcome::Completed(serde_json::json!({"status":200,"body":"{}"})),
            )
            .unwrap();
        let seeded = journal.to_script_value();

        let env = Map::new();
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let invocation = HandlerInvocation {
            body: "fetch('https://api.test/u');",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
            journal: Some(&seeded),
        };

        let err = engine
            .eval_handler_run("routes/flow", &invocation)
            .expect_err("divergence must be loud");
        assert!(
            err.to_string().contains("different request at step 0"),
            "got {err}"
        );
    }

    /// § 11 R6 — credentials reach the request and never the journal. The
    /// digest is method + URL + body, so an `Authorization` header cannot make
    /// two identical calls look different, and a journal dump is not a
    /// credential dump.
    #[test]
    fn headers_travel_with_the_request_but_never_enter_the_digest() {
        use super::QuickJsEngine;
        use crate::runtime::bridge::{HandlerInvocation, HandlerRun};
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
        use serde_json::Map;

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");

        let env = Map::new();
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let digest_of = |engine: &mut QuickJsEngine, body: &'static str| {
            let invocation = HandlerInvocation {
                body,
                is_block: true,
                env: &env,
                raw_bindings: &[],
                setters: &[],
                event_json: None,
                broadcast_current: &bc,
                journal: None,
            };
            match engine.eval_handler_run("routes/flow", &invocation).unwrap() {
                HandlerRun::Suspended { pending, .. } => pending,
                other => panic!("expected a suspension, got {other:?}"),
            }
        };

        let bare = digest_of(&mut engine, "fetch('https://api.test/u');");
        let authed = digest_of(
            &mut engine,
            "fetch('https://api.test/u', { headers: { Authorization: 'Bearer sk_live_secret' } });",
        );

        assert_eq!(
            bare[0].digest, authed[0].digest,
            "headers are outside the digest"
        );
        assert_eq!(
            authed[0].headers,
            vec![("Authorization".to_string(), "Bearer sk_live_secret".to_string())],
            "but they do travel with the request"
        );
    }

    // P6 — a block body's early `return` is CAPTURED as the result, not leaked
    // out of the effect-collection wrapper: a validation-return form action must
    // still report its side-effects AND surface `{ error: ... }`. Guards the
    // fixed return-escape bug in `build_handler_script`.
    #[test]
    fn eval_handler_captures_block_return_alongside_effects() {
        use super::QuickJsEngine;
        use crate::ir::opcode::SlotId;
        use crate::runtime::bridge::HandlerInvocation;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
        use serde_json::Map;

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let env = Map::new();
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let setters = vec![("touch".to_string(), SlotId(4))];
        // A setter fires, THEN the body returns early — pre-fix, the `return`
        // escaped the wrapper and both the effect serialization and the result
        // were lost.
        let invocation = HandlerInvocation {
            body: "touch(1); return { error: { note: 'too short' } };",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &setters,
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };

        let outcome = engine
            .eval_handler("routes/margin", &invocation)
            .expect("handler runs");

        // The pre-return effect survives.
        assert_eq!(outcome.effects.len(), 1);
        // The returned value is captured, not swallowed by the wrapper.
        let result = outcome.result.expect("result captured");
        assert_eq!(result["error"]["note"], serde_json::json!("too short"));
    }

    // The event payload is exposed to the body as `event`.
    #[test]
    fn eval_handler_exposes_event_payload() {
        use super::QuickJsEngine;
        use crate::ir::opcode::SlotId;
        use crate::runtime::bridge::{HandlerEffect, HandlerInvocation};
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
        use serde_json::Map;

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let env = Map::new();
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let setters = vec![("setName".to_string(), SlotId(2))];
        let invocation = HandlerInvocation {
            body: "setName(event.value)",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &setters,
            event_json: Some(r#"{"value":"typed text"}"#),
            broadcast_current: &bc,
            journal: None,
        };

        let effects = engine
            .eval_handler("routes/input", &invocation)
            .expect("handler runs")
            .effects;
        assert_eq!(
            effects[0],
            HandlerEffect::SlotSet {
                slot_id: SlotId(2),
                value: b"\"typed text\"".to_vec()
            }
        );
    }

    // Updater-form broadcast: `broadcast(topic, fn)` reads the seeded current
    // value, applies the updater, and a second call in the same body chains off
    // the first — matching the pure-Rust read-modify-write.
    #[test]
    fn eval_handler_resolves_updater_form_broadcast() {
        use super::QuickJsEngine;
        use crate::runtime::bridge::{HandlerEffect, HandlerInvocation};
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
        use serde_json::Map;

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let env = Map::new();
        let bc = vec![("count".to_string(), b"5".to_vec())];
        let invocation = HandlerInvocation {
            body: "broadcast(\"count\", n => n + 1); broadcast(\"count\", n => n + 1);",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };

        let effects = engine
            .eval_handler("routes/counter", &invocation)
            .expect("updater-form broadcast runs")
            .effects;

        // Seeded at 5: first updater → 6, second chains off 6 → 7.
        assert_eq!(effects.len(), 2);
        match (&effects[0], &effects[1]) {
            (
                HandlerEffect::Broadcast { topic, value, .. },
                HandlerEffect::Broadcast { value: value2, .. },
            ) => {
                assert_eq!(topic, "count");
                assert_eq!(value, b"6");
                assert_eq!(value2, b"7");
            }
            other => panic!("expected two broadcast effects, got {other:?}"),
        }
    }

    // ── A1 · host-object bridge — renders under QuickJS ───────────────────

    // Before this slice a `import { useState } from "react"` component threw
    // `MODULE_MISSING` at load (the import rewrote to `__albedo_require("react")`).
    // Now `react`/`albedo` imports bind to the global hook shims, so a real hook
    // component LOADS and RENDERS, falling back to each hook's initial when the
    // host seed carries no value for it.
    #[test]
    fn react_use_state_component_renders_with_initial() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            import { useState } from "react";
            export default function Counter(props) {
                const [count, setCount] = useState(props.start);
                return <span data-role="count">{count}</span>;
            }
        "#;
        engine
            .load_module("routes/counter.tsx", src)
            .expect("hook component loads under quickjs");

        let out = engine
            .render_component("routes/counter.tsx", r#"{"start":7}"#)
            .expect("hook component renders");
        assert_eq!(out.html, "<span data-role=\"count\">7</span>");
    }

    // Async server component (RSC) — the default export is `async` and `await`s a
    // data function before returning JSX. The host drives the QuickJS job queue to
    // resolution (`MaybePromise::finish`), so the render is the *awaited* HTML, not
    // `String(Promise)` → "[object Promise]". This server-side await is what makes
    // async server components renderable at all.
    #[test]
    fn async_server_component_is_awaited_on_render() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            async function getStats() {
                return { commits: 1284, repos: 37 };
            }
            export default async function Stats() {
                const s = await getStats();
                return <p id="stats-line">{s.commits + " / " + s.repos}</p>;
            }
        "#;
        engine
            .load_module("routes/stats.tsx", src)
            .expect("async component loads under quickjs");

        let out = engine
            .render_component("routes/stats.tsx", "{}")
            .expect("async server component renders");
        assert_eq!(out.html, "<p id=\"stats-line\">1284 / 37</p>");
    }

    // A rejected await inside an async server component must surface as a loud
    // render error carrying the thrown message — never a silent blank (the
    // failure mode that originally shipped: an empty Tier-B placeholder).
    #[test]
    fn async_server_component_rejection_surfaces_loudly() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            export default async function Boom() {
                await Promise.reject(new Error("data fetch failed"));
                return <p>unreachable</p>;
            }
        "#;
        engine
            .load_module("routes/boom.tsx", src)
            .expect("module loads");

        let err = engine
            .render_component("routes/boom.tsx", "{}")
            .expect_err("a rejected await must not render successfully");
        let message = format!("{err:?}");
        assert!(
            message.contains("data fetch failed"),
            "render error must carry the thrown message, got: {message}"
        );
    }

    // A host seed keyed by positional hook index overrides the initial, so the
    // render reflects the current slot value (e.g. after an action wrote it).
    #[test]
    fn host_seed_overrides_use_state_initial() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            import { useState } from "react";
            export default function Counter() {
                const [count] = useState(0);
                return <span>{count}</span>;
            }
        "#;
        engine
            .load_module("routes/counter.tsx", src)
            .expect("module loads");

        let out = engine
            .render_component_with_host("routes/counter.tsx", "{}", r#"{"state":{"0":42}}"#)
            .expect("seeded render");
        assert_eq!(out.html, "<span>42</span>");

        // The seed must not leak: a follow-up host-unaware render uses the initial.
        let plain = engine
            .render_component("routes/counter.tsx", "{}")
            .expect("plain render");
        assert_eq!(plain.html, "<span>0</span>");
    }

    // Two positional hooks line up with the seed by call order.
    #[test]
    fn host_seed_aligns_multiple_hooks_by_index() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            import { useState } from "react";
            export default function Pair() {
                const [a] = useState("a0");
                const [b] = useState("b0");
                return <span>{a}:{b}</span>;
            }
        "#;
        engine.load_module("routes/pair.tsx", src).expect("loads");

        // Seed only the second hook; the first falls back to its initial.
        let out = engine
            .render_component_with_host("routes/pair.tsx", "{}", r#"{"state":{"1":"B"}}"#)
            .expect("seeded render");
        assert_eq!(out.html, "<span>a0:B</span>");
    }

    // `useSharedSlot` (imported from `albedo`) reads the broadcast-backed seed.
    #[test]
    fn use_shared_slot_reads_host_seed() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            import { useSharedSlot } from "albedo";
            export default function Room() {
                const topic = useSharedSlot("chat:room");
                return <span>{topic}</span>;
            }
        "#;
        engine.load_module("routes/room.tsx", src).expect("loads");

        // B4 · the holder is stamped with `data-albedo-slot` by
        // `mark_shared_slot_scalars`. That attribute IS the fix: SSR output for a
        // scalar read carries no `data-albedo-id`, so without it the client has
        // no way to register a paint site and a broadcast `SlotSet` strands in
        // `pendingSlotValues` — the value only ever appeared on reload.
        let out = engine
            .render_component_with_host(
                "routes/room.tsx",
                "{}",
                r#"{"shared":{"chat:room":"hello"}}"#,
            )
            .expect("seeded render");
        assert_eq!(out.html, "<span data-albedo-slot=\"chat:room\">hello</span>");

        // No seed → null binding renders empty (matches the pure-Rust fallback).
        // The stamp still rides, so a later broadcast has somewhere to land.
        let plain = engine
            .render_component("routes/room.tsx", "{}")
            .expect("plain render");
        assert_eq!(plain.html, "<span data-albedo-slot=\"chat:room\"></span>");
    }

    /// The SSR half of the paint rule, as a table. Each row is a value a topic
    /// can hold and the text a stamped holder must show for it — and the client
    /// must produce the same text from the same topic's JSON, or the page
    /// changes shape on the first update without the data changing.
    ///
    /// Two rows are the bug this closed. An object rendered `[object Object]`
    /// and then flipped to JSON the moment anything live arrived, and `[1,2,3]`
    /// rendered `123` — JavaScript's array-join coercion, which reads as data
    /// loss. Both were measured with curl rather than from the DOM: the client
    /// runtime has already overwritten the holder's text by the time a browser
    /// can be asked, so the browser is not a witness to SSR.
    #[test]
    fn a_stamped_scalar_read_renders_by_the_slot_text_rule() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");
        engine
            .load_module(
                "routes/ops.tsx",
                r#"
                import { useSharedSlot } from "albedo";
                export default function Ops() {
                    const status = useSharedSlot("ops");
                    return <span>{status}</span>;
                }
                "#,
            )
            .expect("loads");

        // A topic value is a plain user value, so `h` escapes it before
        // embedding — but as TEXT, which means `&`, `<` and `>` and not `"`.
        //
        // This case previously expected `{&quot;state&quot;:&quot;ok&quot;}`,
        // and settled the discrepancy by declaring that "the agreement is about
        // the text node, not the bytes of the markup" — both spellings do decode
        // to the same DOM text. That concession is what the renderer conformance
        // gate exists to withdraw. The bytes are not incidental: the pure-Rust
        // renderer emits `{"state":"ok"}` here (its `escape_html` leaves quotes
        // alone), the live path writes that same string into the node directly,
        // and the row-delta path compares markup as bytes — so a value that
        // rendered one way at SSR and another way live read as a change that had
        // not happened. Now all three spell it identically.
        for (seeded, expected) in [
            (r#""green""#, "green"),
            ("4242", "4242"),
            ("true", "true"),
            ("null", ""),
            (r#"{"state":"ok"}"#, r#"{"state":"ok"}"#),
            ("[1,2,3]", "[1,2,3]"),
        ] {
            let out = engine
                .render_component_with_host(
                    "routes/ops.tsx",
                    "{}",
                    &format!(r#"{{"shared":{{"ops":{seeded}}}}}"#),
                )
                .expect("seeded render");
            assert_eq!(
                out.html,
                format!("<span data-albedo-slot=\"ops\">{expected}</span>"),
                "seeded {seeded}"
            );
        }
    }

    /// A member read is the only sensible markup for an object-valued topic, so
    /// it renders its member *and* carries the path that makes it live. Before
    /// this, the two available shapes were live-but-wrong and right-but-static.
    #[test]
    fn a_member_read_renders_the_member_and_stamps_its_path() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("engine init");
        engine
            .load_module(
                "routes/ops.tsx",
                r#"
                import { useSharedSlot } from "albedo";
                export default function Ops() {
                    const status = useSharedSlot("ops");
                    return <span>{status.state}</span>;
                }
                "#,
            )
            .expect("loads");

        let out = engine
            .render_component_with_host(
                "routes/ops.tsx",
                "{}",
                r#"{"shared":{"ops":{"state":"ok","depth":3}}}"#,
            )
            .expect("seeded render");
        assert_eq!(
            out.html,
            "<span data-albedo-slot=\"ops\" data-albedo-slot-path=\"state\">ok</span>"
        );

        // A topic that has not arrived yet must not throw mid-render. This is
        // the case that decided where the path is walked: left in the tree as
        // `status.state`, an unresolved topic is `null.state` and the route
        // 500s — which is what the author's own workaround did before this, and
        // widening the marker would have shipped that failure to more pages.
        // Walked by the formatter, it renders empty and goes live when the
        // value arrives, like every other unresolved read here.
        let plain = engine
            .render_component("routes/ops.tsx", "{}")
            .expect("an unresolved topic renders empty rather than throwing");
        assert_eq!(
            plain.html,
            "<span data-albedo-slot=\"ops\" data-albedo-slot-path=\"state\"></span>"
        );

        // A value that is not an object at all takes the same exit — the client
        // answers `''` for this shape too, from its own copy of the walk.
        let scalar = engine
            .render_component_with_host("routes/ops.tsx", "{}", r#"{"shared":{"ops":7}}"#)
            .expect("render");
        assert_eq!(
            scalar.html,
            "<span data-albedo-slot=\"ops\" data-albedo-slot-path=\"state\"></span>"
        );
    }

    /// `key` is React's reconciliation identity, not a valid raw HTML attribute
    /// — it must never ship as `key="1"`. But it IS the delta sink's row
    /// identity, so the shim stamps it as `data-albedo-key` (the QuickJS mirror
    /// of Phase-K's `stamp_row_key`), letting a Tier-B keyed list reconcile.
    /// `className` is asserted alongside to prove real attributes (and their
    /// rename) are untouched.
    #[test]
    fn reserved_props_never_reach_the_rendered_html() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            export default function List() {
                const items = [{ id: 7, label: "first" }, { id: 8, label: "second" }];
                return (
                    <ul className="entries">
                        {items.map((item) => (
                            <li className="entry" key={item.id}>{item.label}</li>
                        ))}
                    </ul>
                );
            }
        "#;
        engine.load_module("routes/list.tsx", src).expect("loads");

        let out = engine
            .render_component("routes/list.tsx", "{}")
            .expect("list render");

        assert!(
            !out.html.contains(" key=\""),
            "raw React `key` must not be emitted as an HTML attribute, got: {}",
            out.html
        );
        assert_eq!(
            out.html,
            "<ul class=\"entries\">\
             <li class=\"entry\" data-albedo-key=\"7\">first</li>\
             <li class=\"entry\" data-albedo-key=\"8\">second</li></ul>",
        );
    }

    /// `ref` is the same class of prop as `key` — a host-node escape hatch, not
    /// an attribute.
    #[test]
    fn a_ref_prop_never_reaches_the_rendered_html() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            export default function Box() {
                return <div ref="anchor" id="real">x</div>;
            }
        "#;
        engine.load_module("routes/box.tsx", src).expect("loads");

        let out = engine
            .render_component("routes/box.tsx", "{}")
            .expect("box render");

        assert_eq!(out.html, "<div id=\"real\">x</div>");
    }

    // The wider hook surface (useEffect/useRef/useMemo/useCallback) neither
    // fails to load nor crashes a render — effects are no-ops on the server.
    #[test]
    fn full_hook_surface_renders_without_crashing() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            import { useState, useEffect, useRef, useMemo, useCallback } from "react";
            export default function Widget(props) {
                const [n] = useState(props.n);
                const ref = useRef(null);
                const doubled = useMemo(function() { return n * 2; }, [n]);
                const cb = useCallback(function() { return n; }, [n]);
                useEffect(function() { ref.current = n; }, [n]);
                return <span>{doubled}:{typeof cb}</span>;
            }
        "#;
        engine.load_module("routes/widget.tsx", src).expect("loads");

        let out = engine
            .render_component("routes/widget.tsx", r#"{"n":5}"#)
            .expect("renders");
        assert_eq!(out.html, "<span>10:function</span>");
    }

    // `useContext` loads and renders on the server. Eager `h` invocation means a
    // nested Provider can't thread its value down in a single SSR pass (the
    // client applies that on hydration), so a consumer resolves the context
    // DEFAULT here — but it must not crash, and `createContext`/`useContext`
    // must resolve as `react` imports.
    #[test]
    fn use_context_renders_default_without_crashing() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let src = r#"
            import { createContext, useContext } from "react";
            const ThemeContext = createContext("light");
            function Label() {
                const theme = useContext(ThemeContext);
                return <span>{theme}</span>;
            }
            export default function App(props) {
                return <ThemeContext.Provider value="dark"><Label /></ThemeContext.Provider>;
            }
        "#;
        engine.load_module("routes/app.tsx", src).expect("loads");

        let out = engine
            .render_component("routes/app.tsx", "{}")
            .expect("renders");
        // Consumer reads the createContext default ("light") server-side; the
        // Provider value ("dark") is applied client-side on hydration.
        assert_eq!(out.html, "<span>light</span>");
    }

    // Island client-reference boundary. A Tier-C island reached from a
    // server-rendered (async) parent is compiled to a client reference: its
    // server-graph module body is a stub that returns ONLY the framework's
    // canonical empty island placeholder — island code never runs on the server.
    // This proves the prelude primitive + project-import resolution converge on
    // the exact string the serve-time island fill targets, so an island nested
    // in an `async function Page()` hydrates identically to a Tier-A parent's
    // island child.
    #[test]
    fn island_client_reference_stub_renders_empty_placeholder_in_async_page() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        // The island's server-context module: the client-reference stub (byte
        // shape mirrors `render::tier_b::island_client_reference_stub`).
        engine
            .load_module(
                "components/Progress.tsx",
                "export default (function __albedoIslandRef(props) { \
return globalThis.__albedo_island_placeholder(\"__c_progress_7\"); });",
            )
            .expect("stub loads");

        // An async server page that mounts the island as a child. The island
        // must appear as its placeholder hole, not as executed island markup.
        let page = r#"
            import Progress from "../components/Progress";
            export default async function Page() {
                return <main><h1>Essay</h1><Progress /></main>;
            }
        "#;
        engine
            .load_module("routes/index.tsx", page)
            .expect("page loads");

        let out = engine
            .render_component("routes/index.tsx", "{}")
            .expect("renders");

        assert!(
            out.html
                .contains(r#"<div id="__c_progress_7" data-albedo-tier="c"></div>"#),
            "async page must emit the island's empty placeholder hole, got: {}",
            out.html
        );
        assert!(
            out.html.contains("<h1>Essay</h1>"),
            "the page's own server content still renders: {}",
            out.html
        );
    }

    // Slice 3 — `generateMetadata(props)` evaluates under QuickJS to a plain
    // object: synchronous and async forms, param-dependent, and a clean `None`
    // for routes that declare no such export.
    #[test]
    fn eval_route_metadata_returns_sync_async_and_absent() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        // Sync generateMetadata reading a route param.
        let sync_src = r#"
            export function generateMetadata(props) {
                return { title: "Post " + props.params.slug, description: "the post" };
            }
            export default function Page() { return <main></main>; }
        "#;
        engine
            .load_module("routes/blog/[slug].tsx", sync_src)
            .expect("loads");
        let meta = engine
            .eval_route_metadata("routes/blog/[slug].tsx", r#"{"params":{"slug":"hello"}}"#)
            .expect("eval ok")
            .expect("has metadata");
        assert_eq!(meta["title"], "Post hello");
        assert_eq!(meta["description"], "the post");

        // Async generateMetadata is awaited to settlement on the server.
        let async_src = r#"
            export async function generateMetadata(props) {
                return { title: "Async " + props.params.id };
            }
            export default function Page() { return <main></main>; }
        "#;
        engine
            .load_module("routes/item/[id].tsx", async_src)
            .expect("loads");
        let meta = engine
            .eval_route_metadata("routes/item/[id].tsx", r#"{"params":{"id":"42"}}"#)
            .expect("eval ok")
            .expect("has metadata");
        assert_eq!(meta["title"], "Async 42");

        // A module without generateMetadata resolves to None — the static head
        // stands, no error.
        let plain_src = r#"export default function Page() { return <main></main>; }"#;
        engine
            .load_module("routes/plain.tsx", plain_src)
            .expect("loads");
        assert!(engine
            .eval_route_metadata("routes/plain.tsx", "{}")
            .expect("eval ok")
            .is_none());
    }

    // ── Phase L · form-action rewrite under QuickJS ──────────────────
    //
    // A `<form action="action:NAME">` rendered by the QuickJS shim used to ship
    // verbatim: no `data-albedo-action`, and — the part that mattered — no
    // hidden CSRF input, because only the pure-Rust renderer performed the
    // rewrite. The form still submitted (the client runtime recognises the raw
    // sentinel too), so every Tier-B form POSTed with no token and the gate,
    // which only ran when a token was present, let it through.
    //
    // These assert against the shared `transforms::form` constants rather than
    // against literals typed out here. That is the point: the same constants
    // the pure-Rust renderer emits are the ones being checked, so the tests
    // state renderer *parity*, not just "the shim emits some markup".

    fn engine_rendering(specifier: &str, src: &str, props: &str) -> String {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");
        engine.load_module(specifier, src).expect("module loads");
        engine
            .render_component(specifier, props)
            .expect("component renders")
            .html
    }

    #[test]
    fn quickjs_form_action_is_rewritten_and_carries_the_csrf_placeholder() {
        use crate::transforms::form::{CSRF_PLACEHOLDER_INPUT, FORM_ACTION_ATTR};

        let html = engine_rendering(
            "routes/sign.tsx",
            r#"
            export default function Sign() {
                return <form action="action:sign_guestbook"><input name="author" /></form>;
            }
            "#,
            "{}",
        );

        assert!(
            html.contains(&format!("{FORM_ACTION_ATTR}=\"sign_guestbook\"")),
            "the sentinel must be rewritten to the action hook: {html}",
        );
        assert!(
            html.contains(CSRF_PLACEHOLDER_INPUT),
            "a Tier-B form must carry the same CSRF placeholder Tier-A emits: {html}",
        );
        assert!(
            !html.contains("action=\"action:sign_guestbook\""),
            "the raw sentinel must not also ship as an attribute: {html}",
        );
        // First child, so it is serialized with the form like any other field.
        assert!(
            html.contains(&format!(">{CSRF_PLACEHOLDER_INPUT}")),
            "the CSRF input belongs at the head of the form body: {html}",
        );
    }

    /// The seam the whole fix turns on: what the shim emits must be
    /// fillable by the server pass. A shim-only fix would satisfy the
    /// test above and still ship `value=""` to the browser, because the
    /// Tier-B chunk path never ran the substitution — a failure that is
    /// invisible in the markup and only shows up as a rejected submit.
    #[test]
    fn quickjs_form_placeholder_is_fillable_by_the_server_pass() {
        use crate::transforms::form::{fill_csrf_tokens, fill_return_paths};

        let html = engine_rendering(
            "routes/sign2.tsx",
            r#"
            export default function Sign() {
                return <form action="action:sign_guestbook"></form>;
            }
            "#,
            "{}",
        );

        // Both fills, because the server runs both in one pass. Asserting only
        // the CSRF one would let a shim that emits an unfillable return input
        // pass — the same shape of miss this test was written for.
        let served = fill_return_paths(&fill_csrf_tokens(&html, "0123456789abcdef"), "/sign2");
        assert!(
            served.contains("value=\"0123456789abcdef\""),
            "the server fill must find the shim's placeholder: {served}",
        );
        assert!(
            served.contains("value=\"/sign2\""),
            "the return path must be fillable too: {served}",
        );
        assert!(
            !served.contains("value=\"\""),
            "no empty token may reach the browser: {served}",
        );
    }

    /// The no-JS path, asserted on the shim's own output: a Tier-B form must
    /// carry a real endpoint and a forced POST, or a browser with no client
    /// runtime submits to the current page and gets a 405.
    #[test]
    fn a_tier_b_form_action_ships_a_real_endpoint_and_a_forced_post() {
        let html = engine_rendering(
            "routes/sign3.tsx",
            r#"
            export default function Sign() {
                return <form action="action:sign_guestbook" method="get"></form>;
            }
            "#,
            "{}",
        );
        assert!(
            html.contains("action=\"/_albedo/action/sign_guestbook\""),
            "{html}"
        );
        assert!(html.contains("method=\"post\""), "{html}");
        assert!(
            !html.contains("method=\"get\""),
            "an authored GET on an action form must be overwritten, not honoured: {html}"
        );
        // The interceptor's hook survives alongside it — with JS the browser
        // submit never happens, and both paths reach the same handler.
        assert!(
            html.contains("data-albedo-action=\"sign_guestbook\""),
            "{html}"
        );
    }

    /// Parity with `transforms::form::action_endpoint`, pinned behaviourally
    /// because a byte predicate has no JSON representation and the shim
    /// restates the alphabet. A name that cannot be a path segment must ship no
    /// `action` at all on this renderer, exactly as on the pure-Rust one — a
    /// Tier-B form that navigated to `/_albedo/action/..` would be worse than
    /// one that does nothing.
    #[test]
    fn a_form_action_name_that_cannot_be_a_url_segment_ships_no_action_attribute() {
        let html = engine_rendering(
            "routes/sign4.tsx",
            r#"
            export default function Sign() {
                return <form action="action:sign guestbook" method="get"></form>;
            }
            "#,
            "{}",
        );
        assert!(!html.contains("action=\"/_albedo/action/"), "{html}");
        assert!(!html.contains("action=\"action:"), "{html}");
        assert!(
            html.contains("data-albedo-action=\"sign guestbook\""),
            "the JS path still works: {html}"
        );
        assert!(
            html.contains("method=\"get\""),
            "with no endpoint to force, the authored method is left alone: {html}"
        );
    }

    /// React's uncontrolled form-control props (`defaultValue`/`defaultChecked`)
    /// are not HTML attributes — the DOM spells them `value`/`checked`. The shim
    /// must translate them exactly as the pure-Rust `render_attrs` does, or a
    /// pre-filled Tier-B `<input>` renders blank (the browser lowercases a
    /// passed-through `defaultValue` to the inert `defaultvalue`). Parity with
    /// `component::tests::render_attrs_translates_uncontrolled_form_props`.
    #[test]
    fn quickjs_translates_uncontrolled_form_props_to_html_attributes() {
        let html = engine_rendering(
            "routes/edit.tsx",
            r#"
            export default function Edit() {
                return <p>
                    <input name="score" defaultValue="200" />
                    <input type="checkbox" defaultChecked={true} />
                </p>;
            }
            "#,
            "{}",
        );

        assert!(
            html.contains("value=\"200\""),
            "defaultValue must render as the DOM `value` attribute: {html}",
        );
        assert!(
            !html.contains("defaultValue"),
            "the React prop name must not reach the browser: {html}",
        );
        // Boolean → bare attribute, the same path a literal `checked` takes.
        assert!(
            html.contains(" checked"),
            "defaultChecked={{true}} must render as bare `checked`: {html}",
        );
        assert!(
            !html.contains("defaultChecked"),
            "the React prop name must not reach the browser: {html}",
        );
    }

    #[test]
    fn quickjs_form_appends_seeded_error_spans_for_its_action() {
        use super::QuickJsEngine;
        use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");
        engine
            .load_module(
                "routes/sign3.tsx",
                r#"
                export default function Sign() {
                    return <form action="action:sign_guestbook">
                        <input name="author" />
                        <input name="message" />
                    </form>;
                }
                "#,
            )
            .expect("module loads");

        // Seed the spans the way `render_entry_quickjs_inner` does — keyed by
        // action name. The ids here are arbitrary in this shim-only test; the
        // compiled-project test proves they equal `allocate_field_error_id`.
        let host = r#"{"formErrorSpans":{"sign_guestbook":"<span data-albedo-id=\"111\" data-albedo-error=\"author\"></span><span data-albedo-id=\"222\" data-albedo-error=\"message\"></span>"}}"#;
        let html = engine
            .render_component_with_host("routes/sign3.tsx", "{}", host)
            .expect("component renders")
            .html;

        assert!(
            html.contains(r#"<span data-albedo-id="111" data-albedo-error="author"></span>"#),
            "the author error sink must be present: {html}",
        );
        assert!(
            html.contains(r#"<span data-albedo-id="222" data-albedo-error="message"></span>"#),
            "the message error sink must be present: {html}",
        );
        // Spans land inside the form (before its close tag), so a SetText that
        // targets them by id finds a node under the form subtree.
        let close = html.find("</form>").expect("form closes");
        assert!(
            html.find(r#"data-albedo-error="message""#).unwrap() < close,
            "error sinks must sit inside the form: {html}",
        );
    }

    #[test]
    fn quickjs_form_emits_no_error_spans_without_a_seed() {
        // The host-less render path (no `formErrorSpans`) must not fabricate
        // spans — proves the append is driven by the seed, not hardcoded, and
        // that the existing host-unaware tests stay honest.
        let html = engine_rendering(
            "routes/sign4.tsx",
            r#"
            export default function Sign() {
                return <form action="action:sign_guestbook"><input name="author" /></form>;
            }
            "#,
            "{}",
        );
        assert!(
            !html.contains("data-albedo-error"),
            "no seed means no error sinks: {html}",
        );
    }

    #[test]
    fn quickjs_leaves_a_plain_html_form_untouched() {
        use crate::transforms::form::{CSRF_MARKER_ATTR, FORM_ACTION_ATTR};

        let html = engine_rendering(
            "routes/plain_form.tsx",
            r#"
            export default function Search() {
                return <form action="/search" method="get"></form>;
            }
            "#,
            "{}",
        );

        // Not an Albedo action: it keeps its native submit behaviour and never
        // gains `data-albedo-action`. It gains no token either — a GET form puts
        // its fields in the URL, so a token here would land in the history and
        // the access log. (A same-origin *POST* form does get one; see
        // `a_plain_post_form_gets_the_hidden_inputs_through_the_shim`.)
        assert!(html.contains("action=\"/search\""), "{html}");
        assert!(!html.contains(FORM_ACTION_ATTR), "{html}");
        assert!(!html.contains(CSRF_MARKER_ATTR), "{html}");
    }

    /// The Tier-B half of what makes a sign-in form authorable. The pure-Rust
    /// renderer's copy of this rule is unit-tested in `transforms::form`; this
    /// is the parity check that the shim agrees, which is the failure mode the
    /// whole served-markup contract exists to prevent — the QuickJS path once
    /// emitted no CSRF input at all and the gate waved it through.
    #[test]
    fn a_plain_post_form_gets_the_hidden_inputs_through_the_shim() {
        use crate::transforms::form::{FORM_ACTION_ATTR, FORM_HIDDEN_INPUTS};

        let html = engine_rendering(
            "routes/login.tsx",
            r#"
            export default function Login() {
                return <form action="/_albedo/auth/password/login" method="POST"></form>;
            }
            "#,
            "{}",
        );

        assert!(
            html.contains(FORM_HIDDEN_INPUTS),
            "a same-origin POST form must carry both hidden inputs: {html}",
        );
        // The author's URL is the one that posts. Nothing is rewritten and there
        // is no action name for a client interceptor to key on.
        assert!(
            html.contains("action=\"/_albedo/auth/password/login\""),
            "{html}",
        );
        assert!(!html.contains(FORM_ACTION_ATTR), "{html}");
    }

    /// The disclosure half, through the shim: an off-origin POST form must not
    /// be handed this session's token to submit elsewhere.
    #[test]
    fn an_off_origin_post_form_gets_no_token_through_the_shim() {
        use crate::transforms::form::CSRF_MARKER_ATTR;

        for action in ["https://evil.example/collect", "//evil.example/collect"] {
            let html = engine_rendering(
                "routes/leak.tsx",
                &format!(
                    r#"
                    export default function Leak() {{
                        return <form action="{action}" method="POST"></form>;
                    }}
                    "#
                ),
                "{}",
            );
            assert!(
                !html.contains(CSRF_MARKER_ATTR),
                "{action} must not receive a token: {html}",
            );
        }
    }
}
