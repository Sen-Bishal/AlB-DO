use super::arena::{ArenaAllocator, ArenaControl, ArenaStats};
use super::bridge::{
    build_handler_script, decode_handler_envelope, HandlerEffect, HandlerInvocation,
};
use super::engine::{
    stable_source_hash, BootstrapPayload, LoadErrorKind, RenderOutput, RuntimeEngine, RuntimeError,
    RuntimeResult,
};
use rquickjs::{promise::MaybePromise, Context, Function, Runtime};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use swc_common::{
    comments::SingleThreadedComments, sync::Lrc, FileName, Globals, Mark, SourceMap, Span, Spanned,
    GLOBALS,
};
use swc_ecma_ast::{
    Decl, ExportSpecifier, ImportSpecifier, Module, ModuleDecl, ModuleExportName, ModuleItem, Pat,
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
    /// Runs under the same request-scoped arena discipline as a render: after
    /// warmup the body bump-allocates into the request region, the effect JSON
    /// is copied out into Rust, then the boundary reset reclaims the region.
    pub fn eval_handler(
        &mut self,
        entry: &str,
        invocation: &HandlerInvocation,
    ) -> RuntimeResult<Vec<HandlerEffect>> {
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
        decode_handler_envelope(entry, &envelope_json)
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
        if code.len() > MAX_MODULE_SIZE {
            return Err(RuntimeError::load(
                LoadErrorKind::EngineFailure,
                format!(
                    "Module '{specifier}' exceeds maximum size limit of {} bytes",
                    MAX_MODULE_SIZE
                ),
            ));
        }

        let code_hash = stable_source_hash(code);
        if self.loaded_module_hashes.get(specifier).copied() == Some(code_hash) {
            return Ok(());
        }

        self.ensure_initialized()?;
        let script = compile_module_script_for_quickjs(specifier, code)?;

        self.context.as_ref().unwrap().with(|ctx| {
            ctx.eval::<(), _>(script.as_str()).map_err(|err| {
                RuntimeError::load(
                    LoadErrorKind::EngineFailure,
                    format!("failed to load module '{specifier}': {err}"),
                )
            })
        })?;

        self.loaded_module_hashes
            .insert(specifier.to_string(), code_hash);
        Ok(())
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
                    format!("failed to load precompiled module '{specifier}': {err}"),
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
fn build_npm_runtime_helpers_script() -> String {
    format!(
        r#"
(function() {{
  if (typeof globalThis.__albedo_require_record === 'function') {{ return; }}
  globalThis.__ALBEDO_NPM_FACTORIES = Object.create(null);
  globalThis.__ALBEDO_NPM_ALIASES = Object.create(null);
  if (typeof globalThis.process === 'undefined') {{
    globalThis.process = {{ env: {{ NODE_ENV: 'production' }} }};
  }}
  const __albedo_has_own = Object.prototype.hasOwnProperty;
  const __albedo_is_record = function(candidate) {{
    return candidate !== null
      && typeof candidate === 'object'
      && candidate.{MODULE_RECORD_FLAG} === true;
  }};

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
}})();
"#
    )
}

fn build_builtin_runtime_helpers_script() -> &'static str {
    r#"
if (typeof globalThis.h !== 'function') {
  const __albedo_escape_html = function(str) {
    return String(str).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#x27;');
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
    out.push(new AlbedoHtml(__albedo_escape_html(String(value))));
  };

  const h = function(type, props, ...children) {
    const flatChildren = [];
    __albedo_push_children(children, flatChildren);

    if (typeof type === 'function') {
      const mergedProps = Object.assign({}, props || {});
      if (flatChildren.length === 1) {
        mergedProps.children = flatChildren[0];
      } else if (flatChildren.length > 1) {
        mergedProps.children = flatChildren;
      }
      return type(mergedProps);
    }

    let attrs = '';
    const safeProps = props || {};
    for (const key in safeProps) {
      if (!Object.prototype.hasOwnProperty.call(safeProps, key) || key === 'children') {
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
      // JSX prop → HTML attribute rename, mirroring the pure-Rust renderer
      // (`render_attrs`) so QuickJS-rendered islands match build-time Tier-A
      // markup and apply CSS classes in the browser.
      const attrName = key === 'className' ? 'class' : key;
      if (value === true) {
        attrs += ' ' + attrName;
        continue;
      }
      attrs += ' ' + attrName + '="' + __albedo_escape_html(value) + '"';
    }

    const inner = flatChildren.join('');
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

  globalThis.h = h;
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
    let normalized = code.trim();
    if normalized.is_empty() {
        return Err(RuntimeError::load(
            LoadErrorKind::EngineFailure,
            format!("module '{specifier}' is empty"),
        ));
    }

    let transpiled = transpile_module_source_for_quickjs(specifier, normalized)?;

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
    let lowered = lower_module_to_statements(specifier, source, rewrite_import_declaration)?;
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

type ImportRewriter = fn(swc_ecma_ast::ImportDecl, &str) -> RuntimeResult<Vec<String>>;

fn lower_module_to_statements(
    specifier: &str,
    source: &str,
    rewrite_import: ImportRewriter,
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

/// Tier-C client island import policy: framework runtime imports bind to the
/// globals the client runtime installs (mirroring the server-side
/// [`rewrite_framework_runtime_import`]); anything else is rejected loudly,
/// because npm packages and child-component modules are not yet client-bundled
/// (the A3.2 vendor-chunk follow-up).
fn rewrite_import_for_client(
    import_decl: swc_ecma_ast::ImportDecl,
    specifier: &str,
) -> RuntimeResult<Vec<String>> {
    let import_source = import_decl.src.value.to_string();
    if is_framework_runtime_import(import_source.as_str()) {
        return rewrite_framework_runtime_import(import_decl, specifier);
    }
    Err(RuntimeError::load(
        LoadErrorKind::UnsupportedSyntax,
        format!(
            "Tier-C client island '{specifier}' imports '{import_source}', which is not yet \
             bundled for the browser; only framework runtime imports (react/react-dom/albedo) \
             resolve client-side today (npm + child-module client chunks are the A3.2 follow-up)"
        ),
    ))
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
    let normalized = source.trim_start_matches('\u{feff}');
    let transpiled = transpile_module_source_for_quickjs(specifier, normalized)?;

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
        lower_module_to_statements(specifier, transpiled.as_str(), rewrite_import_for_client)?
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

fn transpile_module_source_for_quickjs(specifier: &str, source: &str) -> RuntimeResult<String> {
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let preferred_syntax = syntax_for_specifier(specifier);
        let (mut module, source_map) =
            parse_module_with_fallback(specifier, source, preferred_syntax)?;

        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        module.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, false));
        module.visit_mut_with(&mut strip_type());

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
}

fn rewrite_import_declaration(
    import_decl: swc_ecma_ast::ImportDecl,
    specifier: &str,
) -> RuntimeResult<Vec<String>> {
    let import_source = import_decl.src.value.to_string();

    if is_framework_runtime_import(import_source.as_str()) {
        return rewrite_framework_runtime_import(import_decl, specifier);
    }

    let import_source_literal = js_string_literal(import_source.as_str(), specifier)?;

    // Side-effect import: still trigger module initialization.
    if import_decl.specifiers.is_empty() {
        return Ok(vec![format!(
            "__albedo_import_namespace({import_source_literal});"
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
                    "const {local} = __albedo_import_default({import_source_literal});"
                ));
            }
            ImportSpecifier::Namespace(namespace_specifier) => {
                let local = namespace_specifier.local.sym.to_string();
                statements.push(format!(
                    "const {local} = __albedo_import_namespace({import_source_literal});"
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
            "const {{ {} }} = __albedo_import_named({import_source_literal});",
            named_bindings.join(", ")
        ));
    }

    Ok(statements)
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

    RuntimeError::render(format!("failed to render component '{entry}': {message}"))
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

        let watermark = engine.arena_stats().persistent_used;
        assert!(
            watermark > 0,
            "warmup should have populated the persistent region"
        );

        for i in 0..STEADY {
            let out = engine
                .render_component("routes/stress", props)
                .expect("steady render");
            // Correctness across resets: byte-identical output every time.
            assert_eq!(out.html, expected, "render {i} diverged");

            let stats = engine.arena_stats();
            // The request region is reclaimed wholesale between renders.
            assert_eq!(
                stats.request_used, 0,
                "request region not reset after render {i}"
            );
            // Zero per-tick persistent growth in steady state.
            assert_eq!(
                stats.persistent_used, watermark,
                "persistent region grew on steady-state render {i}"
            );
        }

        // The render region actually carried real per-render traffic (the bump path ran),
        // and never spilled to the system fallback.
        let final_stats = engine.arena_stats();
        assert!(
            final_stats.request_peak > 0,
            "request region was never exercised"
        );
        assert_eq!(
            final_stats.fallback_allocs, 0,
            "region capacity was exceeded"
        );
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
        let bc = Map::new();
        let invocation = HandlerInvocation {
            body,
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &setters,
            event_json: None,
            broadcast_current: &bc,
        };

        let effects = engine
            .eval_handler("routes/counter", &invocation)
            .expect("handler runs");

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
        let bc = Map::new();
        let invocation = HandlerInvocation {
            body: "throw new Error('handler exploded')",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
        };

        let err = engine
            .eval_handler("routes/boom", &invocation)
            .expect_err("a throw must propagate");
        assert!(
            err.to_string().contains("handler exploded"),
            "error should carry the thrown message, got: {err}"
        );
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
        let bc = Map::new();
        let setters = vec![("setName".to_string(), SlotId(2))];
        let invocation = HandlerInvocation {
            body: "setName(event.value)",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &setters,
            event_json: Some(r#"{"value":"typed text"}"#),
            broadcast_current: &bc,
        };

        let effects = engine
            .eval_handler("routes/input", &invocation)
            .expect("handler runs");
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
        use serde_json::{Map, Value};

        let mut engine = QuickJsEngine::new();
        engine
            .init(&BootstrapPayload::default())
            .expect("engine init");

        let env = Map::new();
        let mut bc = Map::new();
        bc.insert("count".to_string(), Value::from(5));
        let invocation = HandlerInvocation {
            body: "broadcast(\"count\", n => n + 1); broadcast(\"count\", n => n + 1);",
            is_block: true,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
        };

        let effects = engine
            .eval_handler("routes/counter", &invocation)
            .expect("updater-form broadcast runs");

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

        let out = engine
            .render_component_with_host(
                "routes/room.tsx",
                "{}",
                r#"{"shared":{"chat:room":"hello"}}"#,
            )
            .expect("seeded render");
        assert_eq!(out.html, "<span>hello</span>");

        // No seed → null binding renders empty (matches the pure-Rust fallback).
        let plain = engine
            .render_component("routes/room.tsx", "{}")
            .expect("plain render");
        assert_eq!(plain.html, "<span></span>");
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
}
