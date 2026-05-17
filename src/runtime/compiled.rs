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
use crate::runtime::eval::component::fnv1a_32;
use crate::runtime::eval::{ComponentFunction, ParamBinding};
use crate::runtime::eval::{ComponentProject, PatchReport};
use crate::runtime::slot_store::SessionSlotView;
use crate::transforms::events::{
    collect_free_idents_in_handler_body, HandlerBody, HandlerExtract,
};
use crate::transforms::hooks::{extract_use_state_hooks, HookBinding, HookExtractError};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use swc_ecma_ast::Stmt;

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

/// Phase-K facade over [`ComponentProject`]. Owns the per-component
/// hook metadata, the handler registry, and the slot/proxy id
/// allocator. The Phase-J `ComponentProject` is exposed verbatim so
/// callers that don't need hook compilation can keep using it.
pub struct CompiledProject {
    project: ComponentProject,
    components: HashMap<(String, String), CompiledComponent>,
    handlers: HashMap<u32, ResolvedHandler>,
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
                    },
                );
            }
        }

        Ok(Self { project, components, handlers })
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
        Ok(report)
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
        let html = compiled.project.render_entry(entry, props)?;
        return Ok(RenderOutput { html, opcodes: Vec::new() });
    }

    let (html, opcodes) = compiled
        .project
        .render_entry_compiled(entry, props, compiled, slots)?;
    Ok(RenderOutput { html, opcodes })
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
