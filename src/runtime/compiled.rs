//! Phase K · the compile-time wrapper around [`ComponentProject`].
//!
//! Combines the Phase-K extractors (`transforms::hooks`, `transforms::events`)
//! with deterministic slot/proxy-id allocation and a render entry that
//! emits binding opcodes alongside HTML. Also owns the handler
//! registry the server-side action dispatcher consults.
//!
//! The architectural choice (see `transforms::mod`): "compiled to a
//! render function" is implemented as **interpretation against
//! pre-extracted metadata**, not source-to-source codegen. Same wire
//! opcodes; reuses the Phase-J evaluator end-to-end.

use crate::ir::action::ActionEnvelope;
use crate::ir::opcode::{Instruction, SlotId};
use crate::runtime::bridge::{HandlerEffect, HandlerInvocation};
use crate::runtime::broadcast::BroadcastRegistry;
use crate::runtime::engine::RuntimeEngine;
use crate::runtime::eval::component::fnv1a_32;
use crate::runtime::quickjs_engine::QuickJsEngine;
use crate::runtime::eval::{ComponentFunction, ParamBinding};
use crate::runtime::eval::{ComponentProject, PatchReport};
use crate::runtime::slot_store::SessionSlotView;
use crate::transforms::actions::ActionDeclaration;
use crate::transforms::css_modules::{is_css_module_path, scope_module_css, ScopedCssModule};
use crate::transforms::events::{
    collect_free_idents_in_handler_body, HandlerBody, HandlerExtract,
};
use crate::transforms::form::{
    allocate_form_action_id, extract_forms_in_function, FormExtract,
};
use crate::transforms::hooks::{extract_use_state_hooks, HookBinding, HookExtractError};
use crate::transforms::link::{extract_links_in_function, LinkExtract};
use crate::transforms::shared_slots::{
    extract_shared_slot_hooks, SharedSlotBinding, SharedSlotExtractError,
};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use swc_common::{sync::Lrc, SourceMap, DUMMY_SP};
use swc_ecma_ast::{Expr, ExprStmt, Module, ModuleItem, Stmt};
use swc_ecma_codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};

/// Per-render knobs the corpus and demo use to opt in to hook
/// compilation. Phase K Stage 1 ships with `hook_compile: false` as
/// the default; the gate flips after the corpus is green and any
/// downstream consumer can override per-call.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    /// When `true`, the renderer:
    ///   * Reads `useState` value bindings from `SessionSlotView`
    ///     (initialising the slot from the initial expression on
    ///     first access).
    ///   * Emits `BindEvent` opcodes for every JSX `on*` handler.
    ///   * Emits `SetTextRef` opcodes for every slot read used in
    ///     a JSX expression context.
    ///
    /// When `false`, the renderer behaves exactly as Phase J shipped:
    /// the useState shim from `eval/core.rs` binds the initial
    /// literal and no binding opcodes are emitted.
    pub hook_compile: bool,
}

/// One render's HTML + the binding opcodes the client needs to
/// hydrate it. The opcode vector is exactly what bakabox expects on
/// the WT patches stream: `BindEvent` to attach handlers, `SetTextRef`
/// / `SetAttrRef` to subscribe slot-bound nodes to the slot store.
#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    pub html: String,
    pub opcodes: Vec<Instruction>,
}

/// One compiled component's metadata, derived from its parsed AST.
#[derive(Debug, Clone)]
pub struct CompiledComponent {
    pub module_spec: String,
    pub function_name: String,
    pub hooks: Vec<HookBinding>,
    pub handlers: Vec<HandlerExtract>,
    /// Map from useState value-binding name → its allocated SlotId.
    /// Slot ids are deterministic: `fnv1a_32("{module}::{fn}#{hook_idx}")`.
    pub value_slots: HashMap<String, SlotId>,
    /// Map from useState setter-binding name → the same SlotId as the
    /// value (a setter writes the slot the value reads from).
    pub setter_slots: HashMap<String, SlotId>,
    /// Handler proxy ids in source order; index parallel to `handlers`.
    /// `fnv1a_32("{module}::{fn}::{event}#{handler_idx}")`.
    pub proxy_ids: Vec<u32>,
    /// Stage 2 — component prop binding names extracted from the
    /// function signature (destructured form only for now). A prop
    /// named here is eligible for capture by any handler that
    /// references it as a free variable.
    pub param_names: Vec<String>,
    /// Stage 2 — captured-prop slot ids keyed by prop name. Each
    /// captured prop gets one render-scoped slot the renderer writes
    /// at every render so the server-side handler dispatch can read
    /// the most recent value back. Slot id is
    /// `fnv1a_32("{module}::{fn}#prop:{name}")` — disjoint from the
    /// hook-slot namespace (`#0`, `#1`, ...) so collisions are
    /// impossible.
    pub capture_slots: HashMap<String, SlotId>,
    /// Phase L · form-action metadata extracted from this component's
    /// JSX. Each entry is one `<form action="action:NAME">` in
    /// source-traversal order. The renderer consults this when it
    /// stamps `data-albedo-action` and the hidden CSRF placeholder;
    /// the server can warn at registration time when a form references
    /// an action name no handler is bound to.
    pub forms: Vec<FormExtract>,
    /// Phase L · `<Link href>` metadata in source-traversal order.
    /// The renderer rewrites each `<Link>` it sees as an `<a href
    /// data-albedo-link>` host element; this list is the diagnostic
    /// surface for tooling that wants to enumerate routes the JSX
    /// references.
    pub links: Vec<LinkExtract>,
    /// Phase O.2 · `useSharedSlot("topic")` calls extracted from the
    /// JSX in source-traversal order. The renderer (next session)
    /// will read the topic's current value via the broadcast
    /// registry; for now this exists as the compile-time contract
    /// downstream wiring compiles against.
    pub shared_slots: Vec<SharedSlotBinding>,
}

/// One handler the server can re-execute when bakabox POSTs an action
/// envelope keyed on `proxy_id`.
#[derive(Debug, Clone)]
pub struct ResolvedHandler {
    pub module_spec: String,
    pub function_name: String,
    pub handler_idx: usize,
    pub event_name: String,
    pub body: HandlerBody,
}

/// Phase P · Stream E.3 — CSS-module class maps + scoped CSS bodies
/// owned by the project. Populated once at [`CompiledProject::wrap`]
/// time by reading every `.module.css` file the project's modules
/// import. The renderer reads from this via a per-thread install
/// (see `eval/core.rs::install_phase_k_css_modules`); the manifest
/// builder reads from this via [`scoped_css_for_module`] to inject
/// `<style>` blocks into each route's shell.
#[derive(Debug, Clone, Default)]
pub struct CssModuleRegistry {
    /// Project-relative path of the `.module.css` file → its scoped
    /// output (class map + rewritten CSS). The key is normalised
    /// with `/` separators so dedup works across components that
    /// share the same file from different import-source spellings.
    files: HashMap<String, ScopedCssModule>,
    /// `module_spec → (binding_name → file_key)`. `binding_name` is
    /// the local name from `import styles from "./X.module.css"`;
    /// `file_key` is the key into [`files`].
    bindings: HashMap<String, HashMap<String, String>>,
}

impl CssModuleRegistry {
    /// Phase P · E.3 — resolve `styles.foo` at render time. Returns
    /// the scoped class name (without leading `.`) or `None` when
    /// `binding` doesn't name a CSS-module import for `module_spec`,
    /// or when `class_name` isn't a declared class in that file.
    pub fn scoped_class_for(
        &self,
        module_spec: &str,
        binding: &str,
        class_name: &str,
    ) -> Option<&str> {
        let file_key = self.bindings.get(module_spec)?.get(binding)?;
        let scoped = self.files.get(file_key)?;
        scoped.class_map.get(class_name).map(|s| s.as_str())
    }

    /// Phase P · E.3 — every CSS-module file's scoped CSS body
    /// referenced by `module_spec`, in stable order. The manifest
    /// builder concatenates the results into one `<style>` block per
    /// route. Empty when the module imports no `.module.css` files.
    pub fn scoped_css_for_module(&self, module_spec: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        if let Some(map) = self.bindings.get(module_spec) {
            // Stable, dedup-by-file order: sort by file_key first
            // so multiple bindings to the same file collapse to one
            // emission. (BTreeSet would dedupe; we use a manual
            // pass so the order is alphabetic.)
            let mut keys: Vec<&String> = map.values().collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                if let Some(scoped) = self.files.get(key) {
                    out.push(scoped.scoped_css.as_str());
                }
            }
        }
        out
    }

    /// Phase P · E.3 — total number of distinct CSS-module files
    /// loaded by the project. Useful for diagnostic surfaces and
    /// tests that want to assert "this project loaded N modules".
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Phase P · E.3 — iterate every loaded file's `(key, scoped)`
    /// pair. The key is the project-relative path with `/`
    /// separators; the value is the full scoping result.
    pub fn iter_files(&self) -> impl Iterator<Item = (&str, &ScopedCssModule)> {
        self.files.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// Phase-K facade over [`ComponentProject`]. Owns the per-component
/// hook metadata, the handler registry, and the slot/proxy id
/// allocator. The Phase-J `ComponentProject` is exposed verbatim so
/// callers that don't need hook compilation can keep using it.
pub struct CompiledProject {
    project: ComponentProject,
    components: HashMap<(String, String), CompiledComponent>,
    handlers: HashMap<u32, ResolvedHandler>,
    /// Phase P · Stream C.1 — every `export const NAME = action(...)`
    /// declaration across the project, keyed by module spec for
    /// per-module diagnostic queries. The wire registration lives in
    /// `handlers` (keyed by `action_id = FNV-1a-32(name)`), parallel
    /// to how Phase K's JSX `on*` handlers register by `proxy_id`.
    action_declarations: HashMap<String, Vec<ActionDeclaration>>,
    /// Phase P · Stream E.3 — CSS-module scoping done once at wrap
    /// time. Render reads the class maps via thread-local install
    /// in `runtime::eval::core`; the manifest builder reads scoped
    /// CSS bodies for per-route `<style>` injection.
    css_modules: CssModuleRegistry,
}

impl CompiledProject {
    /// Load a project from a directory, parse every component, and
    /// extract Phase-K metadata.
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let project = ComponentProject::load_from_dir(&root)?;
        Self::wrap(project)
    }

    /// Wrap an already-loaded [`ComponentProject`] in the Phase-K
    /// facade. Re-extracts metadata from every component.
    pub fn wrap(project: ComponentProject) -> Result<Self> {
        let mut components = HashMap::new();
        let mut handlers = HashMap::new();

        for (module_spec, module) in project.modules() {
            for (function_name, function) in &module.functions {
                let hook_bindings = extract_use_state_hooks(function, &module.imports)
                    .map_err(|err: HookExtractError| {
                        anyhow!(
                            "hook extraction failed in {module_spec}::{function_name}: {err}"
                        )
                    })?;
                let handler_extracts =
                    crate::transforms::events::extract_handlers_in_function(&function.body_stmts);

                // Phase L · run the two new JSX walkers alongside the
                // hook + handler extractors. The walkers are pure
                // metadata passes; they don't mutate the AST and they
                // share the same source-order indexing convention so
                // the renderer can align by position later.
                let form_extracts = extract_forms_in_function(&function.body_stmts);
                let link_extracts = extract_links_in_function(&function.body_stmts);

                // Phase O.2 · same pattern, surfaces `useSharedSlot`
                // calls as a list of (binding_name, topic) records.
                // Compile-time failure here propagates as an
                // `anyhow!` so misuse blocks the build.
                let shared_slot_bindings =
                    extract_shared_slot_hooks(function, &module.imports).map_err(
                        |err: SharedSlotExtractError| {
                            anyhow!(
                                "shared-slot extraction failed in \
                                 {module_spec}::{function_name}: {err}"
                            )
                        },
                    )?;

                let mut value_slots: HashMap<String, SlotId> = HashMap::new();
                let mut setter_slots: HashMap<String, SlotId> = HashMap::new();
                for binding in &hook_bindings {
                    let slot_id = allocate_slot_id(module_spec, function_name, binding.hook_idx);
                    value_slots.insert(binding.value_name.clone(), slot_id);
                    if let Some(setter) = &binding.setter_name {
                        setter_slots.insert(setter.clone(), slot_id);
                    }
                }

                // Stage 2 · prop binding extraction + capture-slot allocation.
                let param_names = extract_param_names(function);
                let mut capture_slots: HashMap<String, SlotId> = HashMap::new();
                if !param_names.is_empty() {
                    // Union of free variables across every handler in
                    // this component, intersected with param_names. A
                    // prop only gets a capture slot when at least one
                    // handler in this component actually references it
                    // — components whose handlers don't read props
                    // don't pay the snapshot-write cost.
                    let mut captured: HashSet<String> = HashSet::new();
                    for handler in &handler_extracts {
                        let frees = collect_free_idents_in_handler_body(&handler.body);
                        for name in frees {
                            if param_names.contains(&name)
                                && !value_slots.contains_key(&name)
                                && !setter_slots.contains_key(&name)
                            {
                                captured.insert(name);
                            }
                        }
                    }
                    for name in captured {
                        let slot_id = allocate_capture_slot_id(module_spec, function_name, &name);
                        capture_slots.insert(name, slot_id);
                    }
                }

                let mut proxy_ids = Vec::with_capacity(handler_extracts.len());
                for handler in &handler_extracts {
                    let proxy_id = allocate_proxy_id(
                        module_spec,
                        function_name,
                        &handler.event_name,
                        handler.handler_idx,
                    );
                    proxy_ids.push(proxy_id);
                    handlers.insert(
                        proxy_id,
                        ResolvedHandler {
                            module_spec: module_spec.clone(),
                            function_name: function_name.clone(),
                            handler_idx: handler.handler_idx,
                            event_name: handler.event_name.clone(),
                            body: handler.body.clone(),
                        },
                    );
                }

                components.insert(
                    (module_spec.clone(), function_name.clone()),
                    CompiledComponent {
                        module_spec: module_spec.clone(),
                        function_name: function_name.clone(),
                        hooks: hook_bindings,
                        handlers: handler_extracts,
                        value_slots,
                        setter_slots,
                        proxy_ids,
                        param_names,
                        capture_slots,
                        forms: form_extracts,
                        links: link_extracts,
                        shared_slots: shared_slot_bindings,
                    },
                );
            }
        }

        // Phase P · Stream E.3 — collect CSS-module imports across
        // every module. Resolved once here (read + scope) so render
        // and manifest paths can both reuse the cached results.
        // Path resolution: import sources like "./Card.module.css"
        // are joined relative to the module's parent directory and
        // re-normalised to project-relative form, so two components
        // in different dirs that import "./Card.module.css" get
        // separate file_keys and disjoint scoped class names — as
        // they should, because they reference different files.
        let css_modules = build_css_module_registry(&project);

        // Phase P · Stream C.1 — register every TS-side `action()`
        // declaration alongside the Phase K onClick handlers. The
        // `action_id` follows Phase L's FNV-1a-32 family so a JSX
        // `<form action="action:NAME">` and a TS `export const NAME =
        // action(...)` converge on the same wire id without
        // per-project configuration. A synthetic empty
        // `CompiledComponent` carries the dispatch context so
        // `invoke_action`'s `component_meta` lookup resolves —
        // TS actions don't own slot bindings, so all per-component
        // maps are empty.
        let mut action_declarations: HashMap<String, Vec<ActionDeclaration>> = HashMap::new();
        for (module_spec, module) in project.modules() {
            if module.action_declarations.is_empty() {
                continue;
            }
            let mut per_module = Vec::with_capacity(module.action_declarations.len());
            for declaration in &module.action_declarations {
                let action_id = allocate_form_action_id(&declaration.name);
                let synthetic_function = format!("__action__{}", declaration.name);
                let synthetic_component = CompiledComponent {
                    module_spec: module_spec.clone(),
                    function_name: synthetic_function.clone(),
                    hooks: Vec::new(),
                    handlers: Vec::new(),
                    value_slots: HashMap::new(),
                    setter_slots: HashMap::new(),
                    proxy_ids: Vec::new(),
                    param_names: Vec::new(),
                    capture_slots: HashMap::new(),
                    forms: Vec::new(),
                    links: Vec::new(),
                    shared_slots: Vec::new(),
                };
                components.insert(
                    (module_spec.clone(), synthetic_function.clone()),
                    synthetic_component,
                );
                handlers.insert(
                    action_id,
                    ResolvedHandler {
                        module_spec: module_spec.clone(),
                        function_name: synthetic_function,
                        handler_idx: 0,
                        event_name: "action".to_string(),
                        body: declaration.body.clone(),
                    },
                );
                per_module.push(declaration.clone());
            }
            action_declarations.insert(module_spec.clone(), per_module);
        }

        Ok(Self {
            project,
            components,
            handlers,
            action_declarations,
            css_modules,
        })
    }

    /// Underlying Phase-J project; useful for callers that need access
    /// to component scanning, patch reports, or any of the pre-K API.
    #[must_use]
    pub fn project(&self) -> &ComponentProject {
        &self.project
    }

    pub fn patch(
        &mut self,
        changed_paths: &[PathBuf],
        deleted_paths: &[PathBuf],
    ) -> Result<PatchReport> {
        let report = self.project.patch(changed_paths, deleted_paths)?;
        // The hook metadata for changed components needs re-extraction.
        // Cheapest correct path for Phase K Stage 1: rebuild the entire
        // metadata + handler index. A future incremental patch can
        // touch only the components named in `report.reparsed_specifiers`.
        let project_clone = self.project.clone();
        let rebuilt = Self::wrap(project_clone)?;
        self.components = rebuilt.components;
        self.handlers = rebuilt.handlers;
        self.action_declarations = rebuilt.action_declarations;
        self.css_modules = rebuilt.css_modules;
        Ok(report)
    }

    /// Phase P · Stream E.3 — handle on the project's CSS-module
    /// registry. Render reads class maps via a thread-local install
    /// of this `&CssModuleRegistry`; the manifest builder reads
    /// scoped CSS bodies for per-route `<style>` injection.
    pub fn css_modules(&self) -> &CssModuleRegistry {
        &self.css_modules
    }

    /// Look up the metadata for one component. Returns `None` when the
    /// project doesn't contain a matching `(module_spec, function_name)`.
    #[must_use]
    pub fn component_meta(
        &self,
        module_spec: &str,
        function_name: &str,
    ) -> Option<&CompiledComponent> {
        self.components
            .get(&(module_spec.to_string(), function_name.to_string()))
    }

    /// Look up a handler by its `proxy_id`. Returns the resolved AST
    /// body the action dispatcher should execute.
    #[must_use]
    pub fn handler(&self, proxy_id: u32) -> Option<&ResolvedHandler> {
        self.handlers.get(&proxy_id)
    }

    /// Total count of registered handlers — for diagnostic surfaces.
    #[must_use]
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }

    /// Iterate every handler's `proxy_id`. Used by
    /// `AlbedoServerBuilder::register_compiled_project` to bulk-register
    /// adapters into the action dispatcher's `HashMap<u32, _>`.
    pub fn handler_proxy_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.handlers.keys().copied()
    }

    /// Phase O.2 · sorted, deduplicated topic keys referenced by any
    /// `useSharedSlot` call across every component in this project.
    /// Server-side wiring iterates this to pre-register topics with
    /// the [`crate::runtime::BroadcastRegistry`] at startup.
    ///
    /// Sorted output is deterministic so a build-time snapshot of
    /// "topics this project uses" is stable across builds of the
    /// same source.
    pub fn shared_slot_topics(&self) -> Vec<String> {
        let mut topics: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for component in self.components.values() {
            for binding in &component.shared_slots {
                topics.insert(binding.topic.clone());
            }
        }
        topics.into_iter().collect()
    }

    /// Phase O.2 · all `useSharedSlot` bindings for one component
    /// (`{module_spec}::{function_name}`), in source order. Returns
    /// an empty slice for components that don't use shared slots.
    pub fn shared_slots_for_component(
        &self,
        module_spec: &str,
        function_name: &str,
    ) -> &[SharedSlotBinding] {
        self.components
            .get(&(module_spec.to_string(), function_name.to_string()))
            .map(|c| c.shared_slots.as_slice())
            .unwrap_or(&[])
    }

    /// Phase P · Stream C.1 — every `export const NAME = action(...)`
    /// declaration in one module, in source order. Returns an empty
    /// slice when the module declares no actions (or is unknown).
    pub fn action_declarations_for_module(
        &self,
        module_spec: &str,
    ) -> &[ActionDeclaration] {
        self.action_declarations
            .get(module_spec)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Phase P · Stream C.1 — iterate every TS-side action across
    /// every module in the project, yielding `(module_spec, action)`
    /// pairs. Order is module-iteration order (HashMap, not stable
    /// across runs); within each module declarations are in source
    /// order.
    pub fn action_declarations_iter(
        &self,
    ) -> impl Iterator<Item = (&str, &ActionDeclaration)> {
        self.action_declarations
            .iter()
            .flat_map(|(module_spec, decls)| {
                decls.iter().map(move |d| (module_spec.as_str(), d))
            })
    }

    /// Phase P · Stream C.1 — total count of TS-side action
    /// declarations across the project. Useful for diagnostic
    /// surfaces and tests that want to assert "this project
    /// registered N actions" without iterating.
    #[must_use]
    pub fn action_declaration_count(&self) -> usize {
        self.action_declarations
            .values()
            .map(|v| v.len())
            .sum()
    }

    /// Dispatch an [`ActionEnvelope`] against the registered handler.
    /// Returns the explicit `Vec<Instruction>` the handler emits PLUS
    /// the auto-drained `SlotSet` opcodes for any slot the handler
    /// wrote — matching the wire shape `albedo-server::handlers::action`
    /// produces.
    pub fn invoke_action(
        &self,
        envelope: &ActionEnvelope,
        slots: &SessionSlotView,
    ) -> Result<Vec<Instruction>> {
        let handler = self
            .handler(envelope.action_id)
            .ok_or_else(|| anyhow!("no handler registered for action_id {}", envelope.action_id))?;
        let component = self
            .component_meta(&handler.module_spec, &handler.function_name)
            .ok_or_else(|| {
                anyhow!(
                    "handler {} references unknown component {}::{}",
                    envelope.action_id,
                    handler.module_spec,
                    handler.function_name,
                )
            })?;

        let mut explicit = self.project.eval_handler_body(
            &handler.module_spec,
            &handler.body,
            component,
            slots,
        )?;
        explicit.extend(slots.drain_pending());
        Ok(explicit)
    }

    /// Phase P · Stream C.2 — dispatch an action with the broadcast
    /// registry installed for the duration of body evaluation. The
    /// interpreter's `broadcast(topic, updater)` builtin reads from
    /// this registry via a thread-local; when no registry is
    /// installed (the plain [`invoke_action`] path), the builtin
    /// returns a clean error rather than silently no-op-ing.
    ///
    /// The action adapter on the server side (`CompiledProjectActionAdapter`
    /// in `crates/albedo-server/src/server.rs`) is the production
    /// caller — it clones the per-server `Arc<BroadcastRegistry>`
    /// minted by `AlbedoServerBuilder` and threads it through here
    /// per request.
    pub fn invoke_action_with_broadcast(
        &self,
        envelope: &ActionEnvelope,
        slots: &SessionSlotView,
        broadcast: &crate::runtime::broadcast::BroadcastRegistry,
    ) -> Result<Vec<Instruction>> {
        let _broadcast_guard =
            crate::runtime::eval::core::install_phase_k_broadcast(broadcast);
        self.invoke_action(envelope, slots)
    }

    /// A1 · host-object bridge — dispatch an action by running its handler
    /// body under **QuickJS** instead of the pure-Rust interpreter.
    ///
    /// Same lookup + slot contract as [`Self::invoke_action`], but the body
    /// runs in a full JS engine: loops, `try`/`catch`, array methods, and
    /// anything else the Phase-J interpreter rejects now execute. The
    /// component's `useState` values and captured props seed the JS scope (read
    /// from the slot store, falling back to each hook's codegen'd initial
    /// expression when a slot has not been written yet); `setX` setters and
    /// `broadcast` are installed as host bindings. The body's effects come back
    /// in source order, each lowered to the same `Instruction::SlotSet` the
    /// wire already carries. State writes are persisted to the slot store so the
    /// next render sees them, leaving the dirty set clean (the returned vector
    /// already carries every `SlotSet`, so a follow-up drain is a no-op).
    ///
    /// `engine` is borrowed mutably for the dispatch; the caller owns the
    /// engine's lifecycle (per-worker pooling is the server-side concern).
    ///
    /// Module-level constants are seeded into the handler scope (source order,
    /// before state/prop bindings, shadowed names skipped, failing inits → null)
    /// for parity with the pure-Rust path's `seed_env_with_module_constants`.
    pub fn invoke_action_quickjs(
        &self,
        engine: &mut QuickJsEngine,
        envelope: &ActionEnvelope,
        slots: &SessionSlotView,
    ) -> Result<Vec<Instruction>> {
        self.invoke_action_quickjs_inner(engine, envelope, slots, None)
    }

    /// [`Self::invoke_action_quickjs`] with a [`BroadcastRegistry`] so a
    /// handler's `broadcast(topic, value)` call fans the value out to every
    /// other subscribed session (the current session's own view is the local
    /// `SlotSet` in the returned vector). Mirrors the relationship between
    /// [`Self::invoke_action`] and [`Self::invoke_action_with_broadcast`].
    pub fn invoke_action_quickjs_with_broadcast(
        &self,
        engine: &mut QuickJsEngine,
        envelope: &ActionEnvelope,
        slots: &SessionSlotView,
        broadcast: &BroadcastRegistry,
    ) -> Result<Vec<Instruction>> {
        self.invoke_action_quickjs_inner(engine, envelope, slots, Some(broadcast))
    }

    fn invoke_action_quickjs_inner(
        &self,
        engine: &mut QuickJsEngine,
        envelope: &ActionEnvelope,
        slots: &SessionSlotView,
        broadcast: Option<&BroadcastRegistry>,
    ) -> Result<Vec<Instruction>> {
        let handler = self
            .handler(envelope.action_id)
            .ok_or_else(|| anyhow!("no handler registered for action_id {}", envelope.action_id))?;
        let component = self
            .component_meta(&handler.module_spec, &handler.function_name)
            .ok_or_else(|| {
                anyhow!(
                    "handler {} references unknown component {}::{}",
                    envelope.action_id,
                    handler.module_spec,
                    handler.function_name,
                )
            })?;

        let (body_src, is_block) = handler_body_to_js(&handler.body)?;

        // Seed the JS scope. State values and captured props that exist in the
        // store come in as JSON; an unwritten useState value falls back to its
        // initial expression, codegen'd to JS and seeded as engine-trusted
        // source (so `count` resolves on the very first interaction).
        let mut env = serde_json::Map::new();
        let mut raw_bindings: Vec<(String, String)> = Vec::new();

        // Seed module-level constants so a handler referencing one resolves
        // instead of throwing a QuickJS `ReferenceError`. Parity with the
        // pure-Rust `seed_env_with_module_constants`:
        //   * seeded BEFORE state/prop bindings so a `useState(CONST)` initial
        //     and a later const referencing an earlier one both resolve (source
        //     order preserved);
        //   * a const whose name is shadowed by a component-owned binding
        //     (state value / captured prop / setter) is skipped — the pure-Rust
        //     map would overwrite it, and here two `let`s of one name would be a
        //     JS `SyntaxError`, so the component-owned `let` must be the sole one;
        //   * each init is wrapped so a const that fails to evaluate (e.g. it
        //     references an import the handler scope doesn't expose) degrades to
        //     `null` instead of throwing — matching pure-Rust's `unwrap_or(Null)`,
        //     so a failing const never breaks a handler that doesn't use it.
        if let Some(module) = self.project.modules().get(&handler.module_spec) {
            if !module.module_constants.is_empty() {
                let owned: HashSet<&str> = component
                    .value_slots
                    .keys()
                    .chain(component.capture_slots.keys())
                    .chain(component.setter_slots.keys())
                    .map(String::as_str)
                    .collect();
                for (name, expr) in &module.module_constants {
                    if owned.contains(name.as_str()) {
                        continue;
                    }
                    let init = expr_to_js(expr)?;
                    raw_bindings.push((
                        name.clone(),
                        format!("(function(){{try{{return ({init});}}catch(__e){{return null;}}}})()"),
                    ));
                }
            }
        }

        for (name, slot_id) in &component.value_slots {
            if let Some(value) = slots
                .read(*slot_id)
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                env.insert(name.clone(), value);
            } else if let Some(hook) =
                component.hooks.iter().find(|h| &h.value_name == name)
            {
                raw_bindings.push((name.clone(), expr_to_js(&hook.initial)?));
            }
        }
        for (name, slot_id) in &component.capture_slots {
            if let Some(value) = slots
                .read(*slot_id)
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                env.insert(name.clone(), value);
            }
        }

        let setters: Vec<(String, SlotId)> = component
            .setter_slots
            .iter()
            .map(|(name, slot_id)| (name.clone(), *slot_id))
            .collect();

        // The action payload is exposed to the body as `event` when it is valid
        // JSON (form submits, typed-input events); opaque bincode payloads and
        // empty click envelopes pass `None` → the body sees `event === null`.
        let event_string = String::from_utf8(envelope.payload.clone())
            .ok()
            .filter(|s| serde_json::from_str::<Value>(s).is_ok());

        // Seed the pre-write broadcast snapshot so an updater-form
        // `broadcast(topic, fn)` reads the current topic value (parity with the
        // pure-Rust `eval_broadcast_call`, which reads `current_value()` before
        // applying the updater). Only built when broadcast is wired AND the body
        // references `broadcast` — non-broadcasting handlers pay nothing.
        let mut broadcast_current = serde_json::Map::new();
        if let Some(registry) = broadcast {
            if body_src.contains("broadcast") {
                for (topic, bytes) in registry.snapshot_values() {
                    let value = if bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
                    };
                    broadcast_current.insert(topic, value);
                }
            }
        }

        let entry = format!("{}::{}", handler.module_spec, handler.function_name);
        let invocation = HandlerInvocation {
            body: &body_src,
            is_block,
            env: &env,
            raw_bindings: &raw_bindings,
            setters: &setters,
            event_json: event_string.as_deref(),
            broadcast_current: &broadcast_current,
        };

        let effects = engine.eval_handler(&entry, &invocation)?;

        let mut instructions = Vec::with_capacity(effects.len());
        for effect in effects {
            match &effect {
                HandlerEffect::SlotSet { slot_id, value } => {
                    slots.write(*slot_id, value.clone());
                }
                HandlerEffect::Broadcast { topic, value, .. } => {
                    if let Some(registry) = broadcast {
                        // Register the topic if it's ad-hoc (idempotent — an
                        // existing topic keeps its value), so `write_topic`
                        // doesn't fail with `UnknownTopic`. Matches the pure-Rust
                        // path, which `topic()`-seeds before writing.
                        let _ = registry.topic(topic.clone(), b"null".to_vec());
                        // Fan-out to other subscribers; delivery failures
                        // (full/closed session channels) are surfaced by the
                        // registry and are not this handler's concern.
                        let _ = registry.write_topic(topic, value.clone());
                    }
                }
            }
            instructions.push(effect.into_instruction());
        }

        // State writes above marked the slot dirty; the returned vector already
        // carries those SlotSets, so consume and discard the dirty set to keep
        // the post-dispatch invariant (a follow-up drain is a no-op) identical
        // to the pure-Rust `invoke_action` path.
        let _ = slots.drain_pending();

        Ok(instructions)
    }

    /// A1 · host-object bridge (render side) — render an entry component under
    /// **QuickJS** with its host objects exposed.
    ///
    /// The symmetric counterpart to [`Self::invoke_action_quickjs`]: where that
    /// runs a *handler* body in the real engine, this runs the component's
    /// *render* there. The component's original TSX is loaded into `engine`
    /// (cached by source hash) and rendered with a host seed installed:
    ///
    ///   * **props** flow as the component's argument (JSON), exactly as React;
    ///   * **`useState`** values are seeded from the session slot store, keyed
    ///     positionally by hook index — an unwritten slot falls back to the
    ///     hook's initial, parity with the pure-Rust `render_local` seeding;
    ///   * **`useSharedSlot`** values are seeded from the broadcast registry, so
    ///     the render reflects the topic's *current* value, not a stale default.
    ///
    /// This is what lets a component whose render body uses arbitrary JS (loops,
    /// `.map`, `try`, template literals — everything the pure-Rust evaluator
    /// rejects) render correctly while still seeing live host state. It also
    /// snapshots captured props into their slots so a follow-up
    /// [`Self::invoke_action_quickjs`] reads the props this render observed.
    ///
    /// Returns HTML only — the client-hydration binding opcodes still come from
    /// the Phase K pure-Rust emitter ([`render_entry_with_bindings`]); this path
    /// is the SSR HTML payload, not a replacement for that opcode stream.
    pub fn render_entry_quickjs(
        &self,
        engine: &mut QuickJsEngine,
        entry: &str,
        props: &Value,
        slots: &SessionSlotView,
    ) -> Result<RenderOutput> {
        self.render_entry_quickjs_inner(engine, entry, props, slots, None)
    }

    /// [`Self::render_entry_quickjs`] with a [`BroadcastRegistry`] so the render
    /// seeds `useSharedSlot` bindings from the current topic values. Mirrors the
    /// relationship between [`Self::invoke_action_quickjs`] and
    /// [`Self::invoke_action_quickjs_with_broadcast`].
    pub fn render_entry_quickjs_with_broadcast(
        &self,
        engine: &mut QuickJsEngine,
        entry: &str,
        props: &Value,
        slots: &SessionSlotView,
        broadcast: &BroadcastRegistry,
    ) -> Result<RenderOutput> {
        self.render_entry_quickjs_inner(engine, entry, props, slots, Some(broadcast))
    }

    fn render_entry_quickjs_inner(
        &self,
        engine: &mut QuickJsEngine,
        entry: &str,
        props: &Value,
        slots: &SessionSlotView,
        broadcast: Option<&BroadcastRegistry>,
    ) -> Result<RenderOutput> {
        let (module_spec, function_name) = self
            .project
            .resolve_entry_component(entry)
            .ok_or_else(|| anyhow!("could not resolve entry component '{entry}'"))?;

        let source = self
            .project
            .module_source(&module_spec)
            .ok_or_else(|| {
                anyhow!("no retained source for module '{module_spec}' (quickjs render)")
            })?
            .to_string();

        // Idempotent by source hash — re-loading an unchanged module is a no-op
        // inside the engine, so calling this every render is cheap after warmup.
        engine
            .load_module(&module_spec, &source)
            .map_err(|err| anyhow!("failed to load '{module_spec}' for quickjs render: {err}"))?;

        // Build the per-render host seed from the component's compiled metadata.
        let mut host = serde_json::Map::new();
        if let Some(component) = self.component_meta(&module_spec, &function_name) {
            // useState: state[hook_idx] = current slot value. Omitting an index
            // tells the JS shim to use that hook's own initial argument.
            let mut state = serde_json::Map::new();
            for hook in &component.hooks {
                if let Some(slot_id) = component.value_slots.get(&hook.value_name) {
                    if let Some(value) = slots
                        .read(*slot_id)
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    {
                        state.insert(hook.hook_idx.to_string(), value);
                    }
                }
            }
            if !state.is_empty() {
                host.insert("state".to_string(), Value::Object(state));
            }

            // useSharedSlot: shared[topic] = current broadcast value.
            if let Some(registry) = broadcast {
                let mut shared = serde_json::Map::new();
                for binding in &component.shared_slots {
                    if let Some(topic) = registry.get(&binding.topic) {
                        let bytes = topic.current_value();
                        let value = if bytes.is_empty() {
                            Value::Null
                        } else {
                            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
                        };
                        shared.insert(binding.topic.clone(), value);
                    }
                }
                if !shared.is_empty() {
                    host.insert("shared".to_string(), Value::Object(shared));
                }
            }

            // Snapshot captured props into their slots so a handler firing
            // before the next render reads the value the prop had here. Drained
            // immediately — internal bookkeeping, never a user-driven SlotSet —
            // exactly mirroring the pure-Rust `snapshot_captured_props_into_slots`.
            if !component.capture_slots.is_empty() {
                if let Some(props_map) = props.as_object() {
                    for (name, slot_id) in &component.capture_slots {
                        if let Some(value) = props_map.get(name) {
                            if let Ok(bytes) = serde_json::to_vec(value) {
                                slots.write(*slot_id, bytes);
                            }
                        }
                    }
                    let _ = slots.drain_pending();
                }
            }
        }

        let props_json = serde_json::to_string(props)
            .map_err(|err| anyhow!("failed to encode props for quickjs render: {err}"))?;
        let host_json = serde_json::to_string(&Value::Object(host))
            .map_err(|err| anyhow!("failed to encode host seed for quickjs render: {err}"))?;

        let rendered = engine
            .render_component_with_host(&module_spec, &props_json, &host_json)
            .map_err(|err| anyhow!("quickjs render of '{module_spec}' failed: {err}"))?;

        Ok(RenderOutput {
            html: rendered.html,
            opcodes: Vec::new(),
        })
    }
}

// ── A1 · host-object bridge — handler AST → JS source ─────────────────────
//
// The compiled representation keeps a handler body as swc AST
// (`HandlerBody::Expr` / `Block`). Running it under QuickJS needs source text,
// so these helpers codegen the AST back to JS via the same swc emitter the
// engine uses for modules. The AST is the compiler's own (already TS/JSX
// stripped at parse time for handler bodies), so the emitted source is trusted.

/// Emit a sequence of module items as JS source.
fn emit_module_js(items: Vec<ModuleItem>) -> Result<String> {
    let cm: Lrc<SourceMap> = Default::default();
    let module = Module {
        span: DUMMY_SP,
        body: items,
        shebang: None,
    };
    let mut buf = Vec::new();
    {
        let mut emitter = Emitter {
            cfg: CodegenConfig::default(),
            comments: None,
            cm: cm.clone(),
            wr: JsWriter::new(cm, "\n", &mut buf, None),
        };
        emitter
            .emit_module(&module)
            .map_err(|err| anyhow!("failed to codegen handler body: {err}"))?;
    }
    String::from_utf8(buf).map_err(|err| anyhow!("handler codegen produced invalid UTF-8: {err}"))
}

/// Codegen a single expression to JS source with no trailing terminator, so it
/// can be spliced as a sub-expression (`(<expr>)`).
fn expr_to_js(expr: &Expr) -> Result<String> {
    let stmt = Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(expr.clone()),
    });
    let src = emit_module_js(vec![ModuleItem::Stmt(stmt)])?;
    Ok(src.trim().trim_end_matches(';').trim().to_string())
}

/// Codegen a handler body to `(source, is_block)`. An expression body yields a
/// bare expression; a block body yields its statements verbatim.
fn handler_body_to_js(body: &HandlerBody) -> Result<(String, bool)> {
    match body {
        HandlerBody::Expr(expr) => Ok((expr_to_js(expr)?, false)),
        HandlerBody::Block(stmts) => {
            let items = stmts.iter().cloned().map(ModuleItem::Stmt).collect();
            Ok((emit_module_js(items)?, true))
        }
    }
}

/// Render an entry component with hook-compilation enabled.
///
/// When `opts.hook_compile == true`, the returned `RenderOutput`
/// contains:
///   * `html` — the same HTML the Phase-J renderer produces (with
///     `data-albedo-id` stamps on every host element).
///   * `opcodes` — `BindEvent { stable_id, event_id, proxy_id }` for
///     every JSX `on*` handler, and `SetTextRef { stable_id, slot_id }`
///     for every slot-bound expression in a text-child position.
///
/// When `hook_compile == false`, falls back to Phase J behaviour and
/// returns an empty opcode vector.
pub fn render_entry_with_bindings(
    compiled: &CompiledProject,
    entry: &str,
    props: &Value,
    slots: &SessionSlotView,
    opts: &RenderOptions,
) -> Result<RenderOutput> {
    if !opts.hook_compile {
        // Phase P · Stream E.3 — CSS-module class maps must resolve
        // on the Phase J render path too, so Tier-A components that
        // use `styles.foo` emit scoped class names even without hook
        // compile. The guard's lifetime is bounded by the
        // `render_entry` call below; we install + drop within this
        // function so the thread-local doesn't leak.
        let _css_modules_guard =
            crate::runtime::eval::core::install_phase_k_css_modules(
                compiled.css_modules(),
            );
        let html = compiled.project.render_entry(entry, props)?;
        return Ok(RenderOutput { html, opcodes: Vec::new() });
    }

    let (html, opcodes) = compiled
        .project
        .render_entry_compiled(entry, props, compiled, slots)?;
    Ok(RenderOutput { html, opcodes })
}

/// Phase O.2 · render with both the session slot store AND a shared
/// broadcast registry available to `useSharedSlot` bindings. Auto-
/// subscribes `session` to every topic the entry component (or any
/// referenced component) declares via `useSharedSlot`, prepending
/// the initial-value `SlotSet` opcodes onto the returned vector so
/// the freshly-rendered shell paints with the current broadcast
/// state and the client immediately starts receiving fan-outs over
/// its WT patches lane.
///
/// `subscriber_sender` is the same `mpsc::Sender<Vec<u8>>` that
/// feeds the session's `WT_STREAM_SLOT_PATCHES` writer — passing it
/// here is what closes the loop between server-side broadcast writes
/// and client-side patch application.
pub fn render_entry_with_broadcast(
    compiled: &CompiledProject,
    entry: &str,
    props: &Value,
    slots: &SessionSlotView,
    broadcast: &crate::runtime::broadcast::BroadcastRegistry,
    subscriber_sender: crate::runtime::broadcast::BroadcastSender,
    opts: &RenderOptions,
) -> Result<RenderOutput> {
    let topics = compiled.shared_slot_topics();
    // Subscribe BEFORE the render so a write fan-out triggered
    // mid-render (rare but possible if a render callback writes a
    // topic it also reads) reaches this session. The initial
    // SlotSet vector ships out as the head of the opcode response
    // so the client paints with the current broadcast state.
    let initial_opcodes =
        broadcast.auto_subscribe(slots.session_id(), subscriber_sender, &topics);

    if !opts.hook_compile {
        // Phase-J shape: render without binding opcodes. We still
        // ship the initial SlotSets so the client can render shared
        // state immediately even without Phase-K hook compilation.
        // Phase P · E.3 — install the CSS-module class map even on
        // the Phase J path so `styles.foo` resolves for Tier-A
        // components in projects that opted out of hook compile.
        let _css_modules_guard =
            crate::runtime::eval::core::install_phase_k_css_modules(
                compiled.css_modules(),
            );
        let html = compiled.project.render_entry(entry, props)?;
        return Ok(RenderOutput {
            html,
            opcodes: initial_opcodes,
        });
    }

    let (html, mut opcodes) = compiled.project.render_entry_compiled_with_broadcast(
        entry, props, compiled, slots, broadcast,
    )?;
    // Initial SlotSets are prepended so bakabox seeds the shared-slot
    // bindings before any `SetTextRef` referencing them lands.
    let mut combined = Vec::with_capacity(initial_opcodes.len() + opcodes.len());
    combined.extend(initial_opcodes);
    combined.append(&mut opcodes);
    Ok(RenderOutput {
        html,
        opcodes: combined,
    })
}

/// FNV-1a-32 of `"{module_spec}::{function_name}#{hook_idx}"`. The
/// substrate already uses FNV-1a-32 for shell stamping and placeholder
/// ids — reuse the same hash so a downstream debugger sees one
/// consistent id family.
#[must_use]
pub fn allocate_slot_id(module_spec: &str, function_name: &str, hook_idx: usize) -> SlotId {
    let key = format!("{module_spec}::{function_name}#{hook_idx}");
    SlotId(fnv1a_32(key.as_bytes()))
}

/// FNV-1a-32 of `"{module_spec}::{function_name}::{event_name}#{handler_idx}"`.
/// Deterministic across rebuilds of the same source.
#[must_use]
pub fn allocate_proxy_id(
    module_spec: &str,
    function_name: &str,
    event_name: &str,
    handler_idx: usize,
) -> u32 {
    let key = format!("{module_spec}::{function_name}::{event_name}#{handler_idx}");
    fnv1a_32(key.as_bytes())
}

/// Stage 2 · slot id for a captured component prop. Uses the same
/// FNV-1a-32 family as hook slots but with a `#prop:` infix so the
/// allocator namespace cannot collide with `allocate_slot_id`'s
/// `#{hook_idx}` integers. A prop's slot is the same on every render
/// of the same component — last write wins — which is correct for
/// single-instance Stage 2; multi-instance is Stage 3+ work.
#[must_use]
pub fn allocate_capture_slot_id(
    module_spec: &str,
    function_name: &str,
    prop_name: &str,
) -> SlotId {
    let key = format!("{module_spec}::{function_name}#prop:{prop_name}");
    SlotId(fnv1a_32(key.as_bytes()))
}

/// Extract the prop names a component destructures from its single
/// `props` parameter — the conventional shape:
///
/// ```ignore
/// function Stepper({ step, label }: ...) { ... }
/// ```
///
/// yields `["step", "label"]`. Single-binding form (`(props)`) returns
/// an empty list because per-prop names aren't directly destructured;
/// users who need handler-prop capture should destructure for Stage 2.
fn extract_param_names(function: &ComponentFunction) -> Vec<String> {
    let mut out = Vec::new();
    for param in &function.params {
        if let ParamBinding::Object(fields) = param {
            for (_key, local) in fields {
                if !out.contains(local) {
                    out.push(local.clone());
                }
            }
        }
    }
    out
}

/// Helper used by the Phase J `extract` recursion to surface that a
/// `Stmt` exists. Kept here so the public surface remains close to
/// the data types it touches.
#[allow(dead_code)]
pub(crate) fn is_compiled_stmt_root(_stmt: &Stmt) -> bool {
    true
}

/// Phase P · Stream E.3 — walk every module's imports, resolve
/// `.module.css` sources to project-relative paths, read + scope
/// each unique file once. Returns the populated registry.
///
/// Failures (file missing, IO error) skip the binding silently —
/// CSS modules are a presentation concern and a broken CSS file
/// shouldn't fail the build. The renderer falls through to a
/// `Value::Null` member lookup in that case, surfacing as an empty
/// class attribute in the output rather than a panic.
fn build_css_module_registry(project: &ComponentProject) -> CssModuleRegistry {
    let mut registry = CssModuleRegistry::default();
    let root = project.root().to_path_buf();

    for (module_spec, parsed) in project.modules() {
        for (binding_name, import_binding) in &parsed.imports {
            if !is_css_module_path(&import_binding.source) {
                continue;
            }
            // Resolve the import source relative to the module's
            // parent directory. `./Card.module.css` from
            // `src/routes/about/page.tsx` lands at
            // `src/routes/about/Card.module.css`.
            let module_parent = Path::new(module_spec)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let raw_path = module_parent.join(&import_binding.source);
            let file_key = normalize_path_key(&raw_path);
            let abs_path = root.join(&raw_path);
            if !registry.files.contains_key(&file_key) {
                let css_source = match std::fs::read_to_string(&abs_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let scoped = scope_module_css(file_key.as_str(), &css_source);
                registry.files.insert(file_key.clone(), scoped);
            }
            registry
                .bindings
                .entry(module_spec.clone())
                .or_default()
                .insert(binding_name.clone(), file_key);
        }
    }

    registry
}

/// Normalise a path to a stable forward-slash key. `..` segments
/// are collapsed where possible so `./a/../b.css` and `b.css` map
/// to the same entry. Mirrors `eval::component::normalize_specifier`
/// but lives here to avoid a cross-module dependency.
fn normalize_path_key(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            std::path::Component::Normal(seg) => {
                parts.push(seg.to_string_lossy().to_string());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                parts.push(component.as_os_str().to_string_lossy().to_string());
            }
        }
    }
    parts.join("/")
}
