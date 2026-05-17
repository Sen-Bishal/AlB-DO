use crate::ir::opcode::{Instruction, ProxyId, SlotId, StableId};
use crate::runtime::compiled::{
    allocate_proxy_id, CompiledComponent, CompiledProject,
};
use crate::runtime::slot_store::SessionSlotView;
use crate::transforms::events::HandlerBody;
use crate::types::ComponentId;
use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::runtime::eval::component::{
    arg_num, classnames_collect, date_value_ms, escape_html, fnv1a_32, fnv1a_hash,
    import_candidates, is_classnames_source, is_component_module, is_component_tag, is_truthy,
    is_void_tag, json_int, json_num, lit_to_value, make_date_value, normalize_jsx_text,
    normalize_slashes, normalize_specifier, prop_name_to_string, render_attrs, to_number,
    value_to_string,
};

thread_local! {
    /// Per-render element counter. Reset by `render_entry` at the top of
    /// every render call so element ids are deterministic per render and
    /// independent across concurrent renders on different threads.
    ///
    /// Combined with `module_spec` it produces the FNV-1a-32 input that
    /// becomes `data-albedo-id` on every shell element bakabox should be
    /// able to address. Phase K's compiler can replace this with a
    /// content-hash strategy when HMR stability matters.
    static RENDER_ELEMENT_COUNTER: Cell<u32> = const { Cell::new(0) };
}

/// Bakabox reads anchors from `data-albedo-id` (DEFAULT_ANCHOR_ATTRIBUTE
/// in `assets/albedo-runtime.js`). Keep these in sync.
pub const ALBEDO_ID_ATTR: &str = "data-albedo-id";

fn next_element_stable_id(module_spec: &str) -> u32 {
    RENDER_ELEMENT_COUNTER.with(|cell| {
        let counter = cell.get();
        cell.set(counter.wrapping_add(1));
        let key = format!("{module_spec}#{counter}");
        fnv1a_32(key.as_bytes())
    })
}

fn reset_element_counter() {
    RENDER_ELEMENT_COUNTER.with(|cell| cell.set(0));
}

// ─────────────────────────────────────────────────────────────────────
// Phase K · hook-compile thread-local state
//
// `RENDER_K` is populated by `render_entry_compiled` and `eval_handler_body`
// for the duration of one render or handler dispatch. It threads:
//   * the session slot view (for slot reads and writes),
//   * the accumulator for binding opcodes (`BindEvent`, `SetTextRef`),
//   * a stack of containing element `data-albedo-id`s (so a slot read
//     in JSX text-position knows which element to subscribe), and
//   * a stack of per-component hook scopes (so identifier lookup
//     resolves slot-bound names against the right metadata).
// ─────────────────────────────────────────────────────────────────────

thread_local! {
    static RENDER_K: RefCell<Option<RenderKState>> = const { RefCell::new(None) };
}

struct RenderKState {
    slots: SessionSlotView,
    opcodes: Vec<Instruction>,
    element_stack: Vec<u32>,
    scopes: Vec<ComponentScope>,
    /// Render-scoped event intern table. Allocation order is "first
    /// appearance" of each unique event name, starting at id 1 (0 is
    /// reserved for an unset/sentinel id elsewhere in the substrate).
    /// `drain_phase_k_opcodes` prepends a single
    /// `InitInternTable { kind: Event, entries: ... }` opcode so
    /// bakabox can resolve the event_id every `BindEvent` references.
    event_intern: HashMap<String, u16>,
    event_intern_order: Vec<String>,
}

#[derive(Clone)]
struct ComponentScope {
    module_spec: String,
    function_name: String,
    /// Map from value-binding name (`n`) → slot id holding its value.
    value_slots: HashMap<String, SlotId>,
    /// Map from setter-binding name (`setN`) → slot id whose value is
    /// overwritten when the setter is called.
    setter_slots: HashMap<String, SlotId>,
    /// Handler proxy_ids in source order. Indexed by `handlers_emitted`
    /// as the renderer encounters JSX `on*` attributes.
    proxy_ids: Vec<u32>,
    /// Cursor into `proxy_ids` advanced as handlers are emitted.
    handlers_emitted: usize,
    /// Initial-value expressions for each hook in source order. Used
    /// when a slot has not been written yet (first render) to derive
    /// the initial value via the existing Phase-J interpreter.
    initials: Vec<swc_ecma_ast::Expr>,
    /// Map from value-binding name to its position in `initials`. Used
    /// during useState destructure to look up the initial.
    hook_index_for_value: HashMap<String, usize>,
    /// Stage 2 — captured-prop slot ids per prop name. When set, the
    /// renderer writes the current value of each captured prop to its
    /// slot on every render of this component, and
    /// `eval_handler_body` seeds the handler env from these slots so
    /// the handler closure can reference the captured prop.
    capture_slots: HashMap<String, SlotId>,
}

fn phase_k_enabled() -> bool {
    RENDER_K.with(|cell| cell.borrow().is_some())
}

fn phase_k_push_scope(scope: ComponentScope) {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.scopes.push(scope);
        }
    });
}

fn phase_k_pop_scope() {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.scopes.pop();
        }
    });
}

fn phase_k_push_element(stable_id: u32) {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.element_stack.push(stable_id);
        }
    });
}

fn phase_k_pop_element() {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.element_stack.pop();
        }
    });
}

fn phase_k_top_element() -> Option<u32> {
    RENDER_K.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|state| state.element_stack.last().copied())
    })
}

fn phase_k_emit(op: Instruction) {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.opcodes.push(op);
        }
    });
}

fn phase_k_slot_for_value(name: &str) -> Option<SlotId> {
    RENDER_K.with(|cell| {
        cell.borrow().as_ref().and_then(|state| {
            state
                .scopes
                .last()
                .and_then(|scope| scope.value_slots.get(name).copied())
        })
    })
}

fn phase_k_slot_for_setter(name: &str) -> Option<SlotId> {
    RENDER_K.with(|cell| {
        cell.borrow().as_ref().and_then(|state| {
            state
                .scopes
                .last()
                .and_then(|scope| scope.setter_slots.get(name).copied())
        })
    })
}

fn phase_k_next_proxy_id_for_event(event_name: &str) -> Option<u32> {
    RENDER_K.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = borrow.as_mut()?;
        let scope = state.scopes.last_mut()?;
        // The compile pass laid down proxy_ids in source-traversal
        // order; we advance one per emit and verify the recorded
        // event_name matches what the render is asking for. A
        // mismatch means the event order drifted between extraction
        // and render — fail loud rather than silently misroute.
        let idx = scope.handlers_emitted;
        let proxy_id = *scope.proxy_ids.get(idx)?;
        scope.handlers_emitted = idx + 1;
        // Belt-and-suspenders: re-derive the proxy_id from the
        // scope's identity + this event_name + idx, and assert it
        // matches. Mismatch is a programming error in the extractor.
        let derived = allocate_proxy_id(
            &scope.module_spec,
            &scope.function_name,
            event_name,
            idx,
        );
        debug_assert_eq!(
            proxy_id, derived,
            "phase K proxy_id drift: recorded {proxy_id} but derived {derived} for {}::{}::{event_name}#{idx}",
            scope.module_spec, scope.function_name,
        );
        Some(proxy_id)
    })
}

fn phase_k_read_slot_value(slot_id: SlotId) -> Option<Vec<u8>> {
    RENDER_K.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|state| state.slots.read(slot_id))
    })
}

fn phase_k_write_slot_value(slot_id: SlotId, bytes: Vec<u8>) {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow().as_ref() {
            state.slots.write(slot_id, bytes);
        }
    });
}

fn phase_k_current_hook_initial(value_name: &str) -> Option<swc_ecma_ast::Expr> {
    RENDER_K.with(|cell| {
        cell.borrow().as_ref().and_then(|state| {
            let scope = state.scopes.last()?;
            let idx = scope.hook_index_for_value.get(value_name).copied()?;
            scope.initials.get(idx).cloned()
        })
    })
}

/// RAII installer/restorer for the Phase-K thread-local state. Even
/// on panic, the previous (typically `None`) state is reinstated so
/// concurrent renderers on the same thread don't observe leaked state.
struct PhaseKGuard {
    previous: Option<RenderKState>,
}

impl PhaseKGuard {
    fn install(slots: SessionSlotView) -> Self {
        let previous = RENDER_K.with(|cell| {
            cell.replace(Some(RenderKState {
                slots,
                opcodes: Vec::new(),
                element_stack: Vec::new(),
                scopes: Vec::new(),
                event_intern: HashMap::new(),
                event_intern_order: Vec::new(),
            }))
        });
        Self { previous }
    }
}

impl Drop for PhaseKGuard {
    fn drop(&mut self) {
        RENDER_K.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

// `render_local` resolves the current component's scope via
// `current_phase_k_component`, which reads from a thread-local raw
// pointer to the active `CompiledProject` (installed below). The
// pointer is the right trade for keeping the Phase-J `render_*`
// signatures untouched; thread-local-by-pointer is safe because the
// borrow is single-threaded and the guard is dropped before the
// reference goes out of scope on the calling stack frame.
thread_local! {
    static PHASE_K_PROJECT: Cell<Option<*const CompiledProject>> = const { Cell::new(None) };
}

fn install_phase_k_project(project: &CompiledProject) -> PhaseKProjectGuard {
    let previous = PHASE_K_PROJECT.with(|cell| cell.replace(Some(project as *const _)));
    PhaseKProjectGuard { previous }
}

struct PhaseKProjectGuard {
    previous: Option<*const CompiledProject>,
}

impl Drop for PhaseKProjectGuard {
    fn drop(&mut self) {
        PHASE_K_PROJECT.with(|cell| cell.set(self.previous));
    }
}

fn current_phase_k_component(module_spec: &str, function_name: &str) -> Option<ComponentScope> {
    PHASE_K_PROJECT.with(|cell| {
        let ptr = cell.get()?;
        // Safety: the project reference is alive for the duration of
        // the render — `render_entry_compiled` holds `&CompiledProject`
        // on its stack frame while the eval runs, and the guard is
        // dropped before that frame returns. No concurrent mutation
        // is possible because access is thread-local.
        let project = unsafe { &*ptr };
        let meta = project.component_meta(module_spec, function_name)?;
        Some(ComponentScope {
            module_spec: meta.module_spec.clone(),
            function_name: meta.function_name.clone(),
            value_slots: meta.value_slots.clone(),
            setter_slots: meta.setter_slots.clone(),
            proxy_ids: meta.proxy_ids.clone(),
            handlers_emitted: 0,
            initials: meta.hooks.iter().map(|h| h.initial.clone()).collect(),
            hook_index_for_value: meta
                .hooks
                .iter()
                .map(|h| (h.value_name.clone(), h.hook_idx))
                .collect(),
            capture_slots: meta.capture_slots.clone(),
        })
    })
}

fn drain_phase_k_opcodes() -> Vec<Instruction> {
    use crate::ir::opcode::{InternEntry, InternTable, InternTableKind};

    RENDER_K.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(state) = borrow.as_mut() else {
            return Vec::new();
        };
        let body = std::mem::take(&mut state.opcodes);

        // Prepend the event intern table so bakabox can resolve every
        // `event_id` carried by a `BindEvent` opcode below. The control
        // stream conventionally ships intern tables ahead of the
        // referencing opcodes; we honour the same ordering here.
        let mut out: Vec<Instruction> = Vec::with_capacity(body.len() + 1);
        if !state.event_intern_order.is_empty() {
            let entries: Vec<InternEntry> = state
                .event_intern_order
                .iter()
                .enumerate()
                .map(|(idx, name)| InternEntry {
                    id: (idx as u16).saturating_add(1),
                    value: name.clone(),
                })
                .collect();
            out.push(Instruction::InitInternTable {
                table: InternTable {
                    kind: InternTableKind::Event,
                    entries,
                },
            });
        }
        out.extend(body);
        out
    })
}

/// `useState(...)` from `react` AND the current Phase-K scope knows
/// about it. Used by `eval_var_decl_into_env` to decide whether to
/// route through the slot store or fall through to the Phase-J shim.
fn is_use_state_in_phase_k_scope(call: &swc_ecma_ast::CallExpr) -> bool {
    use swc_ecma_ast::*;
    let Callee::Expr(callee) = &call.callee else { return false };
    let Expr::Ident(ident) = callee.as_ref() else { return false };
    ident.sym.as_ref() == "useState" && phase_k_enabled()
}

/// Drain pending dirty entries WITHOUT producing opcodes. Used after
/// a first-render initialisation write so the initial value doesn't
/// show up as a user-driven mutation in the response frame.
fn drain_initial_slot_writes() {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow().as_ref() {
            let _ = state.slots.drain_pending();
        }
    });
}

/// Stage 2 — write the current value of every captured prop into its
/// dedicated capture slot. Called at the top of `render_local` so a
/// handler that fires before the next render still sees the value
/// the prop had on the most recent render.
///
/// Writes are drained immediately because they're internal
/// bookkeeping — surfacing them as `SlotSet` opcodes would push
/// every captured prop down to bakabox on every render, even when
/// the prop didn't change. Bakabox only needs `SlotSet` for slots
/// it has subscribed via `SetTextRef` / `SetAttrRef`; capture slots
/// are never subscribed.
fn snapshot_captured_props_into_slots(scope: &ComponentScope, props: &Value) {
    if scope.capture_slots.is_empty() {
        return;
    }
    let Some(props_map) = props.as_object() else { return };
    for (name, slot_id) in &scope.capture_slots {
        let Some(value) = props_map.get(name) else { continue };
        if let Ok(bytes) = serde_json::to_vec(value) {
            phase_k_write_slot_value(*slot_id, bytes);
        }
    }
    drain_initial_slot_writes();
}

/// Detect whether the expression in a JSX text-position child is a
/// bare slot-bound identifier (e.g. `{n}` for `const [n, setN] =
/// useState(0)`). Returns the SlotId when so, signalling that the
/// renderer should emit a `SetTextRef` binding for the containing
/// element. Phase K Stage 1 only recognises the simple shape; member
/// access (`state.value`), arithmetic, and method calls are Phase J
/// reads and don't subscribe to slot changes.
fn phase_k_detect_slot_text_read(expr: &swc_ecma_ast::Expr) -> Option<SlotId> {
    use swc_ecma_ast::*;
    match expr {
        Expr::Ident(ident) => phase_k_slot_for_value(&ident.sym.to_string()),
        Expr::Paren(paren) => phase_k_detect_slot_text_read(&paren.expr),
        Expr::TsAs(node) => phase_k_detect_slot_text_read(&node.expr),
        Expr::TsNonNull(node) => phase_k_detect_slot_text_read(&node.expr),
        Expr::TsTypeAssertion(node) => phase_k_detect_slot_text_read(&node.expr),
        _ => None,
    }
}

/// Render-scoped event interner. Allocates ids in first-appearance
/// order starting at 1; id 0 is reserved as a sentinel. Bakabox
/// resolves event_id → name through the `InitInternTable` opcode the
/// drain step prepends, so the id only needs to be unique within one
/// render frame.
fn phase_k_event_id_for(event_name: &str) -> crate::ir::opcode::EventId {
    use crate::ir::opcode::EventId;
    RENDER_K.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = match borrow.as_mut() {
            Some(state) => state,
            None => return EventId(0),
        };
        if let Some(id) = state.event_intern.get(event_name) {
            return EventId(*id);
        }
        // Next free id; +1 because 0 is reserved.
        let id = (state.event_intern_order.len() as u16).saturating_add(1);
        state.event_intern.insert(event_name.to_string(), id);
        state.event_intern_order.push(event_name.to_string());
        EventId(id)
    })
}
use crate::runtime::eval::expr::{
    apply_var_pat_to_env, bind_params, bind_params_positional, param_from_pat,
    parse_module as parse_module_impl, ParamBinding, ParsedModule,
};

#[derive(Debug, Clone)]
pub struct ComponentProject {
    root: PathBuf,
    modules: HashMap<String, ParsedModule>,
    source_hashes: HashMap<String, u64>,
    specifier_to_id: HashMap<String, ComponentId>,
    next_id: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PatchReport {
    pub reparsed: usize,
    pub skipped_unchanged: usize,
    pub deleted: usize,
    pub reparsed_ids: Vec<ComponentId>,
    pub reparsed_specifiers: Vec<String>,
    pub deleted_ids: Vec<ComponentId>,
    pub deleted_specifiers: Vec<String>,
}

impl ComponentProject {
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let mut modules = HashMap::new();
        let mut source_hashes = HashMap::new();
        let mut specifier_to_id: HashMap<String, ComponentId> = HashMap::new();
        let mut next_id: u64 = 0;

        for entry in WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            if !is_component_module(path) {
                continue;
            }

            let relative = path
                .strip_prefix(&root)
                .map_err(|err| anyhow!("failed to compute module path: {err}"))?;
            let specifier = normalize_specifier(relative);
            let source = std::fs::read_to_string(path)
                .map_err(|err| anyhow!("failed to read '{}': {err}", path.display()))?;
            let parsed = parse_module_impl(&source, path)?;
            source_hashes.insert(specifier.clone(), fnv1a_hash(source.as_bytes()));
            specifier_to_id.insert(specifier.clone(), ComponentId::new(next_id));
            next_id += 1;
            modules.insert(specifier, parsed);
        }

        if modules.is_empty() {
            return Err(anyhow!("no components found under '{}'", root.display()));
        }

        Ok(Self {
            root,
            modules,
            source_hashes,
            specifier_to_id,
            next_id,
        })
    }

    pub fn patch(
        &mut self,
        changed_paths: &[PathBuf],
        deleted_paths: &[PathBuf],
    ) -> Result<PatchReport> {
        let mut report = PatchReport::default();
        let mut parsed_updates = Vec::new();
        let mut staged_deletions = HashSet::new();
        let mut seen_changed = HashSet::new();

        for changed_path in changed_paths {
            let Some((specifier, absolute_path)) = self.module_specifier_for_path(changed_path)
            else {
                continue;
            };

            if !seen_changed.insert(specifier.clone()) {
                continue;
            }

            match std::fs::read_to_string(&absolute_path) {
                Ok(source) => {
                    let next_hash = fnv1a_hash(source.as_bytes());
                    if self.source_hashes.get(&specifier).copied() == Some(next_hash) {
                        report.skipped_unchanged += 1;
                        continue;
                    }

                    let parsed = parse_module_impl(&source, &absolute_path)?;
                    parsed_updates.push((specifier, parsed, next_hash));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    staged_deletions.insert(specifier);
                }
                Err(err) => {
                    return Err(anyhow!(
                        "failed to read '{}' while patching: {err}",
                        absolute_path.display()
                    ));
                }
            }
        }

        for deleted_path in deleted_paths {
            let Some((specifier, _)) = self.module_specifier_for_path(deleted_path) else {
                continue;
            };
            staged_deletions.insert(specifier);
        }

        for (specifier, parsed, source_hash) in parsed_updates {
            self.modules.insert(specifier.clone(), parsed);
            self.source_hashes.insert(specifier.clone(), source_hash);
            let component_id = *self
                .specifier_to_id
                .entry(specifier.clone())
                .or_insert_with(|| {
                    let id = ComponentId::new(self.next_id);
                    self.next_id += 1;
                    id
                });
            report.reparsed_ids.push(component_id);
            report.reparsed_specifiers.push(specifier);
            report.reparsed += 1;
        }

        for specifier in staged_deletions {
            let component_id = self.specifier_to_id.get(&specifier).copied();
            let removed_module = self.modules.remove(&specifier).is_some();
            let removed_hash = self.source_hashes.remove(&specifier).is_some();
            if removed_module || removed_hash {
                if let Some(component_id) = component_id {
                    report.deleted_ids.push(component_id);
                }
                report.deleted_specifiers.push(specifier);
                report.deleted += 1;
            }
        }

        Ok(report)
    }

    pub fn component_id_for_specifier(&self, specifier: &str) -> Option<ComponentId> {
        let spec = normalize_slashes(specifier);
        self.specifier_to_id.get(&spec).copied()
    }

    pub fn component_id_for_name(&self, name: &str) -> Option<ComponentId> {
        self.specifier_to_id
            .iter()
            .find(|(spec, _)| {
                Path::new(spec)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|stem| stem.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            })
            .map(|(_, &id)| id)
    }

    pub fn component_id_by_name(&self, name: &str) -> Option<ComponentId> {
        self.component_id_for_name(name)
    }

    pub fn render_entry(&self, entry: &str, props: &Value) -> Result<String> {
        // Each top-level render starts with a fresh element counter so the
        // `data-albedo-id` attributes the renderer stamps are stable per
        // render and don't leak across concurrent requests.
        reset_element_counter();
        let entry = self
            .resolve_entry(entry)
            .ok_or_else(|| anyhow!("entry '{}' not found in '{}'", entry, self.root.display()))?;
        self.render_export(&entry, "default", props)
    }

    /// Exposes the parsed-module table so [`CompiledProject`] can run
    /// its Phase-K extractors over every function without re-parsing.
    #[must_use]
    pub fn modules(&self) -> &HashMap<String, ParsedModule> {
        &self.modules
    }

    /// Phase-K render entry: produces HTML plus the binding opcodes
    /// (`BindEvent`, `SetTextRef`) needed to hydrate the rendered
    /// shell against the session slot store. The opcodes ride the
    /// existing WT patches stream when the server ships them.
    ///
    /// Falls back to a Phase-J render when the compiled metadata for
    /// the entry component is empty (no hooks, no handlers).
    pub fn render_entry_compiled(
        &self,
        entry: &str,
        props: &Value,
        compiled: &CompiledProject,
        slots: &SessionSlotView,
    ) -> Result<(String, Vec<Instruction>)> {
        reset_element_counter();
        let entry = self
            .resolve_entry(entry)
            .ok_or_else(|| anyhow!("entry '{}' not found in '{}'", entry, self.root.display()))?;

        // Set up the Phase-K thread-local state. RAII guard restores
        // the previous (None) state even on panic so concurrent
        // renderers on the same thread don't see stale scope. The
        // project guard exposes the compiled metadata to `render_local`
        // via a thread-local pointer — see `current_phase_k_component`.
        let _slot_guard = PhaseKGuard::install(slots.clone());
        let _project_guard = install_phase_k_project(compiled);

        let html = self.render_export(&entry, "default", props)?;
        let opcodes = drain_phase_k_opcodes();
        Ok((html, opcodes))
    }

    /// Re-execute a handler body server-side. The body is whatever
    /// `transforms::events::extract_handlers_in_function` surfaced —
    /// either a single expression (arrow body) or a block of
    /// statements. Setter calls inside the body translate to slot
    /// writes; identifier reads of slot-bound names translate to slot
    /// reads. Returns the explicit `Vec<Instruction>` from the body
    /// (the body itself rarely emits anything explicit — the SlotSet
    /// opcodes come from `SessionSlotView::drain_pending` afterwards).
    pub fn eval_handler_body(
        &self,
        module_spec: &str,
        body: &HandlerBody,
        component: &CompiledComponent,
        slots: &SessionSlotView,
    ) -> Result<Vec<Instruction>> {
        let _guard = PhaseKGuard::install(slots.clone());
        // Push the component's scope. We don't need any of the
        // pre-cache work because a handler only ever runs against one
        // component scope at a time.
        let scope = ComponentScope {
            module_spec: component.module_spec.clone(),
            function_name: component.function_name.clone(),
            value_slots: component.value_slots.clone(),
            setter_slots: component.setter_slots.clone(),
            proxy_ids: component.proxy_ids.clone(),
            handlers_emitted: 0,
            initials: component.hooks.iter().map(|h| h.initial.clone()).collect(),
            hook_index_for_value: component
                .hooks
                .iter()
                .map(|h| (h.value_name.clone(), h.hook_idx))
                .collect(),
            capture_slots: component.capture_slots.clone(),
        };
        phase_k_push_scope(scope);

        // Seed env with the current slot values so identifier reads
        // resolve to live state, not stale literals. The eval will
        // also lazy-load via `phase_k_slot_for_value` when an ident
        // isn't in env, so this is a fast path rather than a
        // correctness gate.
        let mut env: HashMap<String, Value> = HashMap::new();
        for (name, slot_id) in &component.value_slots {
            if let Some(bytes) = slots.read(*slot_id) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    env.insert(name.clone(), value);
                }
            } else if let Some(initial_expr) = component
                .hooks
                .iter()
                .find(|h| &h.value_name == name)
                .map(|h| h.initial.clone())
            {
                let value = self.eval_expr(module_spec, &initial_expr, &env).unwrap_or(Value::Null);
                env.insert(name.clone(), value);
            }
        }

        // Stage 2 — seed env with captured prop snapshots. The render
        // path writes these on every render of the component; here we
        // read them back so the handler body's references to props
        // resolve correctly. Missing snapshots default to Null (the
        // prop was undefined at last render).
        for (name, slot_id) in &component.capture_slots {
            if let Some(bytes) = slots.read(*slot_id) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    env.insert(name.clone(), value);
                }
            }
        }

        let result: Result<Vec<Instruction>> = match body {
            HandlerBody::Expr(expr) => {
                let _ = self.eval_expr(module_spec, expr, &env)?;
                Ok(Vec::new())
            }
            HandlerBody::Block(stmts) => {
                // Evaluate each statement; we only care about side
                // effects (slot writes via setter calls). Returns from
                // a handler are ignored in Phase K Stage 1.
                let mut local_env = env.clone();
                self.eval_body_stmts(module_spec, stmts, &mut local_env)
                    .map(|_| Vec::new())
            }
        };

        phase_k_pop_scope();
        result
    }

    fn resolve_entry(&self, entry: &str) -> Option<String> {
        let entry = normalize_slashes(entry);
        if self.modules.contains_key(&entry) {
            return Some(entry);
        }
        if Path::new(&entry).extension().is_none() {
            for ext in ["jsx", "tsx", "js", "ts"] {
                let candidate = format!("{entry}.{ext}");
                if self.modules.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn module_specifier_for_path(&self, path: &Path) -> Option<(String, PathBuf)> {
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let relative_path = absolute_path.strip_prefix(&self.root).ok()?;
        if !is_component_module(relative_path) {
            return None;
        }
        Some((normalize_specifier(relative_path), absolute_path))
    }

    fn render_export(&self, module_spec: &str, export_name: &str, props: &Value) -> Result<String> {
        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;
        let local = if export_name == "default" {
            module
                .default_export
                .clone()
                .ok_or_else(|| anyhow!("module '{}' has no default export", module_spec))?
        } else {
            export_name.to_string()
        };
        self.render_local(module_spec, &local, props)
    }

    fn render_local(
        &self,
        module_spec: &str,
        function_name: &str,
        props: &Value,
    ) -> Result<String> {
        // Observer frame: opens a cascade-tracking scope for this component's
        // render. The guard publishes a `RenderInfo` on drop iff a process-wide
        // `RenderObserver` is installed — when none is, the whole scope
        // collapses to a single `OnceLock::get()` check.
        let _frame =
            crate::runtime::render_observer::enter_frame_guard(function_name, module_spec);

        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;
        let function = module.functions.get(function_name).ok_or_else(|| {
            anyhow!(
                "function '{}' missing in module '{}'",
                function_name,
                module_spec
            )
        })?;

        let mut env = HashMap::new();
        bind_params(&function.params, props, &mut env);
        let stmts = function.body_stmts.clone();

        // Phase K: push this component's scope if hook-compile is
        // enabled and the compiled project has metadata for it. Pop
        // unconditionally on the way out so panics during eval don't
        // leak scope into a parent component's render.
        let pushed_phase_k_scope = if phase_k_enabled() {
            if let Some(scope) = current_phase_k_component(module_spec, function_name) {
                // Stage 2 · snapshot captured props to their dedicated
                // slots BEFORE evaluating the body, so a handler that
                // fires between renders reads the value the prop had
                // on the most recent render. We drain immediately so
                // the snapshot writes don't surface as user-driven
                // SlotSet opcodes in the response frame.
                snapshot_captured_props_into_slots(&scope, props);
                phase_k_push_scope(scope);
                true
            } else {
                false
            }
        } else {
            false
        };

        let result = self.eval_body_stmts(module_spec, &stmts, &mut env);

        if pushed_phase_k_scope {
            phase_k_pop_scope();
        }
        result
    }

    fn eval_expr(
        &self,
        module_spec: &str,
        expr: &swc_ecma_ast::Expr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        match expr {
            Expr::JSXElement(element) => Ok(Value::String(self.eval_jsx_element(
                module_spec,
                element,
                env,
            )?)),
            Expr::JSXFragment(fragment) => Ok(Value::String(self.eval_jsx_fragment(
                module_spec,
                fragment,
                env,
            )?)),
            Expr::Lit(lit) => Ok(lit_to_value(lit)),
            Expr::Ident(ident) => {
                let name = ident.sym.to_string();
                if let Some(value) = env.get(&name) {
                    Ok(value.clone())
                } else {
                    // Static evaluator has no binding for this identifier.
                    // Phase K wires reactive bindings; until then make the
                    // miss findable in dev rather than letting it vanish.
                    tracing::debug!(
                        target: "albedo::eval",
                        ident = %name,
                        module = %module_spec,
                        "unbound identifier in JSX expression — evaluating to null",
                    );
                    Ok(Value::Null)
                }
            }
            Expr::Member(member) => self.eval_member(module_spec, member, env),
            Expr::Paren(paren) => self.eval_expr(module_spec, &paren.expr, env),
            Expr::Tpl(tpl) => self.eval_tpl(module_spec, tpl, env),
            Expr::Bin(bin) => self.eval_bin(module_spec, bin, env),
            Expr::Cond(cond) => self.eval_cond(module_spec, cond, env),
            Expr::Call(call) => self.eval_call_expr(module_spec, call, env),
            Expr::New(new_expr) => self.eval_new_expr(module_spec, new_expr, env),
            Expr::Array(arr) => self.eval_array_expr(module_spec, arr, env),
            Expr::Object(obj) => self.eval_object_expr(module_spec, obj, env),
            Expr::Unary(unary) => self.eval_unary(module_spec, unary, env),
            Expr::OptChain(opt) => self.eval_opt_chain(module_spec, opt, env),
            Expr::Seq(seq) => {
                let mut last = Value::Null;
                for expr in &seq.exprs {
                    last = self.eval_expr(module_spec, expr, env)?;
                }
                Ok(last)
            }
            // TypeScript escape hatches are runtime no-ops: unwrap to the
            // inner expression. SWC keeps these in the AST when JSX/TSX
            // sources contain `as`, `!`, `<X>e`, `satisfies`, `as const`,
            // or `f<T>` instantiation expressions.
            Expr::TsAs(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsNonNull(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsConstAssertion(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsTypeAssertion(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsSatisfies(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsInstantiation(node) => self.eval_expr(module_spec, &node.expr, env),
            other => {
                // Phase J keeps unhandled shapes returning Null for backwards
                // compatibility, but never silently — every drop emits a
                // tracing event that lets us extend the evaluator. Phase K's
                // SWC pass will compile most of these away into slot-store
                // opcodes, so this list should shrink, not grow.
                tracing::debug!(
                    target: "albedo::eval",
                    module = %module_spec,
                    expr_kind = std::any::type_name_of_val(other),
                    "unhandled JSX expression shape — evaluating to null",
                );
                Ok(Value::Null)
            }
        }
    }

    fn eval_opt_chain(
        &self,
        module_spec: &str,
        opt: &swc_ecma_ast::OptChainExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        match &*opt.base {
            OptChainBase::Member(member) => {
                let obj = self.eval_expr(module_spec, &member.obj, env)?;
                if matches!(obj, Value::Null) {
                    return Ok(Value::Null);
                }
                self.eval_member_on(module_spec, &obj, &member.prop, env)
            }
            OptChainBase::Call(call) => {
                let callee = self.eval_expr(module_spec, &call.callee, env)?;
                if matches!(callee, Value::Null) {
                    return Ok(Value::Null);
                }
                // Callable-value support is Phase K; until then, treat
                // optional calls as null when reachable.
                Ok(Value::Null)
            }
        }
    }

    fn eval_new_expr(
        &self,
        module_spec: &str,
        new_expr: &swc_ecma_ast::NewExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        // Phase J only models `new Date(...)` because that's what ships in
        // the matrix. Other constructors fall through to Null with a trace.
        if let Expr::Ident(ident) = &*new_expr.callee {
            if ident.sym.as_ref() == "Date" {
                let args: Vec<Value> = match &new_expr.args {
                    Some(args) => args
                        .iter()
                        .map(|a| self.eval_expr(module_spec, &a.expr, env))
                        .collect::<Result<Vec<_>>>()?,
                    None => Vec::new(),
                };
                let ms = match args.first() {
                    None => 0.0, // Phase J: deterministic; no system clock.
                    Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
                    Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
                    _ => 0.0,
                };
                return Ok(make_date_value(ms));
            }
        }
        tracing::debug!(
            target: "albedo::eval",
            module = %module_spec,
            "unhandled `new` constructor — evaluating to null",
        );
        Ok(Value::Null)
    }

    fn eval_member(
        &self,
        module_spec: &str,
        member: &swc_ecma_ast::MemberExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let object = self.eval_expr(module_spec, &member.obj, env)?;
        self.eval_member_on(module_spec, &object, &member.prop, env)
    }

    /// Resolve a property access on an already-evaluated value. Factored
    /// out so `Expr::OptChain` and `Expr::Member` share the dispatch.
    fn eval_member_on(
        &self,
        module_spec: &str,
        object: &Value,
        prop: &swc_ecma_ast::MemberProp,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        // Computed access uses the runtime value verbatim — for arrays we
        // want a numeric index without stringifying through `value_to_string`,
        // which would render `1` as `"1"` and lose array-vs-object intent.
        match prop {
            MemberProp::Computed(computed) => {
                let key = self.eval_expr(module_spec, &computed.expr, env)?;
                if let (Value::Array(items), Some(idx)) = (object, key.as_f64()) {
                    if idx.is_finite() && idx >= 0.0 && idx == idx.trunc() {
                        return Ok(items.get(idx as usize).cloned().unwrap_or(Value::Null));
                    }
                }
                let prop_name = value_to_string(&key);
                self.lookup_named_prop(object, &prop_name)
            }
            MemberProp::Ident(ident) => {
                let prop_name = ident.sym.to_string();
                self.lookup_named_prop(object, &prop_name)
            }
            _ => Ok(Value::Null),
        }
    }

    fn lookup_named_prop(&self, object: &Value, prop_name: &str) -> Result<Value> {
        match object {
            Value::Object(map) => {
                // Date-tagged objects expose no JS-level properties; method
                // calls on them are handled in `eval_call_expr` via the
                // member callee path.
                Ok(map.get(prop_name).cloned().unwrap_or(Value::Null))
            }
            Value::Array(items) => match prop_name {
                "length" => Ok(json_int(items.len() as i64)),
                _ => {
                    // Numeric string indexing: `arr["0"]` matches JS semantics.
                    if let Ok(idx) = prop_name.parse::<usize>() {
                        return Ok(items.get(idx).cloned().unwrap_or(Value::Null));
                    }
                    Ok(Value::Null)
                }
            },
            Value::String(s) => match prop_name {
                "length" => Ok(json_int(s.chars().count() as i64)),
                _ => Ok(Value::Null),
            },
            _ => Ok(Value::Null),
        }
    }

    fn eval_tpl(
        &self,
        module_spec: &str,
        tpl: &swc_ecma_ast::Tpl,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let mut result = String::new();
        for (i, quasi) in tpl.quasis.iter().enumerate() {
            let text = quasi
                .cooked
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| quasi.raw.to_string());
            result.push_str(&text);
            if i < tpl.exprs.len() {
                let val = self.eval_expr(module_spec, &tpl.exprs[i], env)?;
                result.push_str(&value_to_string(&val));
            }
        }
        Ok(Value::String(result))
    }

    fn eval_bin(
        &self,
        module_spec: &str,
        bin: &swc_ecma_ast::BinExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        match bin.op {
            BinaryOp::LogicalAnd => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if !is_truthy(&left) {
                    Ok(left)
                } else {
                    self.eval_expr(module_spec, &bin.right, env)
                }
            }
            BinaryOp::LogicalOr => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if is_truthy(&left) {
                    Ok(left)
                } else {
                    self.eval_expr(module_spec, &bin.right, env)
                }
            }
            BinaryOp::NullishCoalescing => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if matches!(left, Value::Null) {
                    self.eval_expr(module_spec, &bin.right, env)
                } else {
                    Ok(left)
                }
            }
            BinaryOp::Add => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                match (&left, &right) {
                    (Value::Number(l), Value::Number(r)) => Ok(json_num(
                        l.as_f64().unwrap_or(0.0) + r.as_f64().unwrap_or(0.0),
                    )),
                    _ => Ok(Value::String(format!(
                        "{}{}",
                        value_to_string(&left),
                        value_to_string(&right)
                    ))),
                }
            }
            BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod | BinaryOp::Exp => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                let l = to_number(&left);
                let r = to_number(&right);
                let value = match bin.op {
                    BinaryOp::Sub => l - r,
                    BinaryOp::Mul => l * r,
                    BinaryOp::Div => l / r,
                    BinaryOp::Mod => l % r,
                    BinaryOp::Exp => l.powf(r),
                    _ => unreachable!(),
                };
                Ok(json_num(value))
            }
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                let l = to_number(&left);
                let r = to_number(&right);
                let result = match bin.op {
                    BinaryOp::Lt => l < r,
                    BinaryOp::Gt => l > r,
                    BinaryOp::LtEq => l <= r,
                    BinaryOp::GtEq => l >= r,
                    _ => unreachable!(),
                };
                Ok(Value::Bool(result))
            }
            BinaryOp::EqEq | BinaryOp::EqEqEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                Ok(Value::Bool(
                    value_to_string(&left) == value_to_string(&right),
                ))
            }
            BinaryOp::NotEq | BinaryOp::NotEqEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                Ok(Value::Bool(
                    value_to_string(&left) != value_to_string(&right),
                ))
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_cond(
        &self,
        module_spec: &str,
        cond: &swc_ecma_ast::CondExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let test = self.eval_expr(module_spec, &cond.test, env)?;
        if is_truthy(&test) {
            self.eval_expr(module_spec, &cond.cons, env)
        } else {
            self.eval_expr(module_spec, &cond.alt, env)
        }
    }

    fn eval_unary(
        &self,
        module_spec: &str,
        unary: &swc_ecma_ast::UnaryExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let val = self.eval_expr(module_spec, &unary.arg, env)?;
        match unary.op {
            UnaryOp::Bang => Ok(Value::Bool(!is_truthy(&val))),
            UnaryOp::Minus => {
                if let Value::Number(n) = &val {
                    Ok(serde_json::Number::from_f64(-n.as_f64().unwrap_or(0.0))
                        .map(Value::Number)
                        .unwrap_or(Value::Null))
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_array_expr(
        &self,
        module_spec: &str,
        arr: &swc_ecma_ast::ArrayLit,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let mut out = Vec::with_capacity(arr.elems.len());
        for elem in &arr.elems {
            if let Some(ExprOrSpread { expr, spread: None }) = elem {
                out.push(self.eval_expr(module_spec, expr, env)?);
            }
        }
        Ok(Value::Array(out))
    }

    fn eval_object_expr(
        &self,
        module_spec: &str,
        obj: &swc_ecma_ast::ObjectLit,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let mut map = serde_json::Map::new();
        for prop in &obj.props {
            if let PropOrSpread::Prop(prop_box) = prop {
                match prop_box.as_ref() {
                    Prop::KeyValue(kv) => {
                        if let Some(key) = prop_name_to_string(&kv.key) {
                            let val = self.eval_expr(module_spec, &kv.value, env)?;
                            map.insert(key, val);
                        }
                    }
                    Prop::Shorthand(ident) => {
                        let name = ident.sym.to_string();
                        let val = env.get(&name).cloned().unwrap_or(Value::Null);
                        map.insert(name, val);
                    }
                    _ => {}
                }
            }
        }
        Ok(Value::Object(map))
    }

    fn eval_call_expr(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;

        // --- Member-callee dispatch: obj.method(...args) -----------------
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Member(member) = callee_expr.as_ref() {
                if let MemberProp::Ident(prop_ident) = &member.prop {
                    let method = prop_ident.sym.to_string();

                    // Static-namespace dispatch (Math.x, Date.x, JSON.x, ...)
                    // is handled before evaluating `member.obj` because the
                    // namespace itself isn't a value we model — `Math.floor`
                    // would otherwise try to look up `Math` in env and miss.
                    if let Expr::Ident(ns_ident) = &*member.obj {
                        let ns_name = ns_ident.sym.to_string();
                        if !env.contains_key(&ns_name) {
                            if let Some(value) = self.eval_static_namespace_call(
                                module_spec,
                                &ns_name,
                                &method,
                                &call.args,
                                env,
                            )? {
                                return Ok(value);
                            }
                        }
                    }

                    // Instance-method dispatch.
                    let obj_val = self.eval_expr(module_spec, &member.obj, env)?;
                    if let Some(value) = self.eval_instance_method(
                        module_spec,
                        &obj_val,
                        &method,
                        &call.args,
                        env,
                    )? {
                        return Ok(value);
                    }
                }
            }
        }

        // --- Bare-ident callee dispatch: f(...args) ----------------------
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Ident(ident) = callee_expr.as_ref() {
                let fn_name = ident.sym.to_string();

                // Phase K · setter dispatch: when the current scope
                // has registered `fn_name` as a useState setter, the
                // call is a slot write — evaluate the arg, JSON-encode
                // it, and store. Returns Null so the handler body's
                // overall value (which is discarded) doesn't surface
                // a confusing string-cast of the written value.
                if let Some(slot_id) = phase_k_slot_for_setter(&fn_name) {
                    let arg_value = match call.args.first() {
                        Some(arg) if arg.spread.is_none() => {
                            self.eval_expr(module_spec, &arg.expr, env)?
                        }
                        _ => Value::Null,
                    };
                    if let Ok(bytes) = serde_json::to_vec(&arg_value) {
                        phase_k_write_slot_value(slot_id, bytes);
                    }
                    return Ok(Value::Null);
                }

                let module = self.modules.get(module_spec);
                let import = module.and_then(|m| m.imports.get(&fn_name));

                // classnames / clsx — flatten args into a class string.
                let is_classnames = import
                    .map(|b| is_classnames_source(&b.source))
                    .unwrap_or(false);
                if is_classnames {
                    let mut classes = Vec::new();
                    for arg in &call.args {
                        if arg.spread.is_some() {
                            continue;
                        }
                        let val = self.eval_expr(module_spec, &arg.expr, env)?;
                        classnames_collect(&val, &mut classes);
                    }
                    return Ok(Value::String(classes.join(" ")));
                }

                // useState shim (Phase J): recognize the React import and
                // return `[initial, null]`. Phase K replaces this with real
                // slot-store reads/writes; until then this lets `{count}`
                // render its initial value instead of vanishing.
                let is_react_use_state = fn_name == "useState"
                    && import
                        .map(|b| b.source == "react" && b.export_name == "useState")
                        .unwrap_or(false);
                if is_react_use_state {
                    let initial = match call.args.first() {
                        Some(arg) if arg.spread.is_none() => {
                            self.eval_expr(module_spec, &arg.expr, env)?
                        }
                        _ => Value::Null,
                    };
                    return Ok(Value::Array(vec![initial, Value::Null]));
                }

                // JS-style coercions.
                if fn_name == "String" || fn_name == "Number" || fn_name == "Boolean" {
                    let arg = match call.args.first() {
                        Some(a) if a.spread.is_none() => {
                            self.eval_expr(module_spec, &a.expr, env)?
                        }
                        _ => Value::Null,
                    };
                    return Ok(match fn_name.as_str() {
                        "String" => Value::String(value_to_string(&arg)),
                        "Number" => json_num(to_number(&arg)),
                        "Boolean" => Value::Bool(is_truthy(&arg)),
                        _ => unreachable!(),
                    });
                }
            }
        }

        Ok(Value::Null)
    }

    fn eval_static_namespace_call(
        &self,
        module_spec: &str,
        ns: &str,
        method: &str,
        args: &[swc_ecma_ast::ExprOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Option<Value>> {
        let evaluated: Vec<Value> = args
            .iter()
            .filter(|a| a.spread.is_none())
            .map(|a| self.eval_expr(module_spec, &a.expr, env))
            .collect::<Result<Vec<_>>>()?;

        let result = match (ns, method) {
            // Math.* — covers everything that shows up in display logic.
            ("Math", "floor") => json_num(arg_num(&evaluated, 0).floor()),
            ("Math", "ceil") => json_num(arg_num(&evaluated, 0).ceil()),
            ("Math", "round") => json_num(arg_num(&evaluated, 0).round()),
            ("Math", "trunc") => json_num(arg_num(&evaluated, 0).trunc()),
            ("Math", "abs") => json_num(arg_num(&evaluated, 0).abs()),
            ("Math", "sqrt") => json_num(arg_num(&evaluated, 0).sqrt()),
            ("Math", "max") => json_num(
                evaluated
                    .iter()
                    .map(to_number)
                    .fold(f64::NEG_INFINITY, f64::max),
            ),
            ("Math", "min") => json_num(
                evaluated
                    .iter()
                    .map(to_number)
                    .fold(f64::INFINITY, f64::min),
            ),
            ("Math", "pow") => json_num(arg_num(&evaluated, 0).powf(arg_num(&evaluated, 1))),

            // Date statics — no system clock in Phase J (deterministic SSR).
            // `Date.now()` returns 0; user code that wants a real timestamp
            // should accept it as a prop. Phase K will surface a clock slot.
            ("Date", "now") => json_int(0),

            // JSON.* — useful in display-time templates for debug surfaces.
            ("JSON", "stringify") => match evaluated.first() {
                Some(value) => Value::String(serde_json::to_string(value).unwrap_or_default()),
                None => Value::Null,
            },

            // Object.keys / Object.values — used in admin/debug UIs.
            ("Object", "keys") => match evaluated.first() {
                Some(Value::Object(map)) => {
                    Value::Array(map.keys().cloned().map(Value::String).collect())
                }
                _ => Value::Array(Vec::new()),
            },
            ("Object", "values") => match evaluated.first() {
                Some(Value::Object(map)) => Value::Array(map.values().cloned().collect()),
                _ => Value::Array(Vec::new()),
            },

            _ => return Ok(None),
        };

        Ok(Some(result))
    }

    fn eval_instance_method(
        &self,
        module_spec: &str,
        receiver: &Value,
        method: &str,
        args: &[swc_ecma_ast::ExprOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Option<Value>> {
        // Date instance methods first — Date is encoded as a tagged object.
        if let Some(ms) = date_value_ms(receiver) {
            return Ok(Some(self.eval_date_method(method, ms)));
        }

        match receiver {
            Value::String(s) => {
                let result = match method {
                    "toUpperCase" => Some(Value::String(s.to_uppercase())),
                    "toLowerCase" => Some(Value::String(s.to_lowercase())),
                    "trim" => Some(Value::String(s.trim().to_string())),
                    "trimStart" | "trimLeft" => Some(Value::String(s.trim_start().to_string())),
                    "trimEnd" | "trimRight" => Some(Value::String(s.trim_end().to_string())),
                    "toString" => Some(Value::String(s.clone())),
                    _ => None,
                };
                Ok(result)
            }
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                let evaluated: Vec<Value> = args
                    .iter()
                    .filter(|a| a.spread.is_none())
                    .map(|a| self.eval_expr(module_spec, &a.expr, env))
                    .collect::<Result<Vec<_>>>()?;
                let result = match method {
                    "toFixed" => {
                        let digits = arg_num(&evaluated, 0).clamp(0.0, 100.0) as usize;
                        Some(Value::String(format!("{:.*}", digits, f)))
                    }
                    "toString" => {
                        let radix = if evaluated.is_empty() {
                            10.0
                        } else {
                            arg_num(&evaluated, 0)
                        };
                        if radix == 10.0 {
                            Some(Value::String(value_to_string(receiver)))
                        } else if (radix - radix.trunc()).abs() < f64::EPSILON
                            && (2.0..=36.0).contains(&radix)
                            && f.is_finite()
                            && f == f.trunc()
                        {
                            let int = f as i64;
                            let radix = radix as u32;
                            let mut digits = String::new();
                            let (sign, mut value) = if int < 0 {
                                ("-", (-(int as i128)) as u128)
                            } else {
                                ("", int as u128)
                            };
                            if value == 0 {
                                digits.push('0');
                            }
                            while value > 0 {
                                let d = (value % radix as u128) as u32;
                                let ch = std::char::from_digit(d, radix).unwrap_or('0');
                                digits.insert(0, ch);
                                value /= radix as u128;
                            }
                            Some(Value::String(format!("{sign}{digits}")))
                        } else {
                            Some(Value::String(value_to_string(receiver)))
                        }
                    }
                    _ => None,
                };
                Ok(result)
            }
            Value::Array(items) => match method {
                "map" => {
                    if let Some(swc_ecma_ast::ExprOrSpread {
                        expr: mapper,
                        spread: None,
                    }) = args.first()
                    {
                        let parts = items
                            .iter()
                            .enumerate()
                            .map(|(i, item)| {
                                self.eval_closure(module_spec, mapper, item, i, env)
                                    .map(|v| value_to_string(&v))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        return Ok(Some(Value::String(parts.join(""))));
                    }
                    Ok(Some(Value::Null))
                }
                "join" => {
                    let sep = match args.first() {
                        Some(a) if a.spread.is_none() => {
                            value_to_string(&self.eval_expr(module_spec, &a.expr, env)?)
                        }
                        _ => ",".to_string(),
                    };
                    let parts: Vec<String> = items.iter().map(value_to_string).collect();
                    Ok(Some(Value::String(parts.join(&sep))))
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn eval_date_method(&self, method: &str, ms: f64) -> Value {
        match method {
            "getTime" | "valueOf" => json_num(ms),
            "toISOString" | "toJSON" | "toString" => {
                Value::String(value_to_string(&make_date_value(ms)))
            }
            _ => Value::Null,
        }
    }

    fn eval_closure(
        &self,
        module_spec: &str,
        expr: &swc_ecma_ast::Expr,
        arg: &Value,
        index: usize,
        parent_env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;

        match expr {
            Expr::Arrow(arrow) => {
                let params: Vec<ParamBinding> = arrow.params.iter().map(param_from_pat).collect();
                let mut env = parent_env.clone();
                let index_val = serde_json::Number::from_f64(index as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
                let args = Value::Array(vec![arg.clone(), index_val]);
                bind_params_positional(&params, &args, &mut env);
                match &*arrow.body {
                    BlockStmtOrExpr::BlockStmt(block) => self
                        .eval_body_stmts(module_spec, &block.stmts, &mut env)
                        .map(Value::String),
                    BlockStmtOrExpr::Expr(body_expr) => {
                        self.eval_expr(module_spec, body_expr, &env)
                    }
                }
            }
            Expr::Fn(fn_expr) => {
                let params: Vec<ParamBinding> = fn_expr
                    .function
                    .params
                    .iter()
                    .map(|p| param_from_pat(&p.pat))
                    .collect();
                let mut env = parent_env.clone();
                let index_val = serde_json::Number::from_f64(index as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
                let args = Value::Array(vec![arg.clone(), index_val]);
                bind_params_positional(&params, &args, &mut env);
                if let Some(body) = &fn_expr.function.body {
                    self.eval_body_stmts(module_spec, &body.stmts, &mut env)
                        .map(Value::String)
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_body_stmts(
        &self,
        module_spec: &str,
        stmts: &[swc_ecma_ast::Stmt],
        env: &mut HashMap<String, Value>,
    ) -> Result<String> {
        use swc_ecma_ast::*;

        for stmt in stmts {
            match stmt {
                Stmt::Return(ret) => {
                    let value = if let Some(expr) = &ret.arg {
                        self.eval_expr(module_spec, expr, env)?
                    } else {
                        Value::Null
                    };
                    return Ok(value_to_string(&value));
                }
                Stmt::Decl(Decl::Var(var)) => {
                    self.eval_var_decl_into_env(module_spec, var, env);
                }
                _ => {}
            }
        }
        Ok(String::new())
    }

    fn eval_var_decl_into_env(
        &self,
        module_spec: &str,
        var: &swc_ecma_ast::VarDecl,
        env: &mut HashMap<String, Value>,
    ) {
        use swc_ecma_ast::*;

        for decl in &var.decls {
            // Phase K hook-compile path: when `const [name, setter] =
            // useState(initial)` is recognised AND the current scope
            // has metadata for `name`, bind `name` to the current slot
            // value (initialising the slot from `initial` on first
            // access). The setter binding stays as Null in env — the
            // call site is intercepted by `eval_call_expr`.
            if let Pat::Array(array) = &decl.name {
                if let Some(init) = &decl.init {
                    if let Expr::Call(call) = init.as_ref() {
                        if is_use_state_in_phase_k_scope(call) {
                            let value_name = array
                                .elems
                                .first()
                                .and_then(|opt| opt.as_ref())
                                .and_then(|p| match p {
                                    Pat::Ident(ident) => Some(ident.id.sym.to_string()),
                                    _ => None,
                                });
                            let setter_name = array
                                .elems
                                .get(1)
                                .and_then(|opt| opt.as_ref())
                                .and_then(|p| match p {
                                    Pat::Ident(ident) => Some(ident.id.sym.to_string()),
                                    _ => None,
                                });
                            if let Some(name) = value_name {
                                if let Some(slot_id) = phase_k_slot_for_value(&name) {
                                    let value = match phase_k_read_slot_value(slot_id) {
                                        Some(bytes) => serde_json::from_slice::<Value>(&bytes)
                                            .unwrap_or(Value::Null),
                                        None => {
                                            // First read for this slot — seed it from
                                            // the initial expression so the state
                                            // persists across re-renders.
                                            let initial_expr = phase_k_current_hook_initial(&name);
                                            let initial_value = initial_expr
                                                .as_ref()
                                                .map(|expr| {
                                                    self.eval_expr(module_spec, expr, env)
                                                        .unwrap_or(Value::Null)
                                                })
                                                .unwrap_or(Value::Null);
                                            if let Ok(bytes) =
                                                serde_json::to_vec(&initial_value)
                                            {
                                                phase_k_write_slot_value(slot_id, bytes);
                                                // Drain immediately — first-render
                                                // initialisations are not user-visible
                                                // mutations, they shouldn't show up
                                                // in the response opcode frame as a
                                                // SlotSet. Drop the pending entries
                                                // by draining and ignoring.
                                                drain_initial_slot_writes();
                                            }
                                            initial_value
                                        }
                                    };
                                    env.insert(name.clone(), value);
                                    if let Some(setter) = setter_name {
                                        env.insert(setter, Value::Null);
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                }
            }

            // Phase J fallback: existing behaviour.
            let value = if let Some(init) = &decl.init {
                self.eval_expr(module_spec, init, env)
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            apply_var_pat_to_env(&decl.name, value, env);
        }
    }

    fn eval_jsx_fragment(
        &self,
        module_spec: &str,
        fragment: &swc_ecma_ast::JSXFragment,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        self.render_children(module_spec, &fragment.children, env, false)
    }

    fn eval_jsx_element(
        &self,
        module_spec: &str,
        element: &swc_ecma_ast::JSXElement,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        use swc_ecma_ast::*;

        let tag = match &element.opening.name {
            JSXElementName::Ident(ident) => ident.sym.to_string(),
            _ => return Err(anyhow!("unsupported JSX tag in module '{}'", module_spec)),
        };

        if is_component_tag(&tag) {
            let mut props = Map::new();
            for (name, value) in self.read_attrs(module_spec, &element.opening.attrs, env)? {
                if !name.starts_with("on") {
                    props.insert(name, value);
                }
            }

            let children = self.read_children_as_values(module_spec, &element.children, env)?;
            if !children.is_empty() {
                if children.len() == 1 {
                    props.insert("children".to_string(), children[0].clone());
                } else {
                    props.insert("children".to_string(), Value::Array(children));
                }
            }

            return self.render_component_ref(module_spec, &tag, &Value::Object(props));
        }

        let mut attrs = self.read_attrs(module_spec, &element.opening.attrs, env)?;

        // Shell-stamp every host (lowercase-tag) element with a stable
        // `data-albedo-id`. Bakabox's `seedNodesFromDocument` looks for
        // exactly this attribute (DEFAULT_ANCHOR_ATTRIBUTE) at boot, so
        // this is the single contract that makes any future Tier-B/C
        // patch addressable. The id is derived BEFORE children render so
        // counter ordering is pre-order and matches client-side traversal.
        //
        // We don't override an explicit user-supplied `data-albedo-id`,
        // which lets test harnesses or static fragments pin a known id.
        let stable_id = match attrs
            .iter()
            .find(|(name, _)| name == ALBEDO_ID_ATTR)
            .and_then(|(_, value)| value.as_str())
            .and_then(|s| s.parse::<u32>().ok())
        {
            Some(existing) => existing,
            None => {
                let id = next_element_stable_id(module_spec);
                attrs.push((
                    ALBEDO_ID_ATTR.to_string(),
                    Value::String(id.to_string()),
                ));
                id
            }
        };

        // Phase K · emit BindEvent for every JSX `on*` handler attached
        // to this element. The proxy_ids were allocated at compile
        // time in source order; the per-scope cursor (`handlers_emitted`)
        // advances one per emit. event_id is the host-level event name
        // — the wire opcode carries the same lowercase string bakabox
        // already maps via `addEventListener`.
        if phase_k_enabled() {
            for attr in &element.opening.attrs {
                if let JSXAttrOrSpread::JSXAttr(jsx_attr) = attr {
                    if let JSXAttrName::Ident(name_ident) = &jsx_attr.name {
                        let name = name_ident.sym.to_string();
                        if name.starts_with("on") && name.len() > 2 {
                            let event_name = name[2..].to_ascii_lowercase();
                            if let Some(proxy_id) =
                                phase_k_next_proxy_id_for_event(&event_name)
                            {
                                phase_k_emit(Instruction::BindEvent {
                                    stable_id: StableId(stable_id),
                                    event_id: phase_k_event_id_for(&event_name),
                                    proxy_id: ProxyId(proxy_id),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Push the element onto the scope-stack so a slot read inside
        // this element's children knows which `stable_id` to subscribe.
        phase_k_push_element(stable_id);

        let attrs_html = render_attrs(&attrs);
        let children_html = self.render_children(module_spec, &element.children, env, false)?;
        let void_tag = is_void_tag(&tag);

        phase_k_pop_element();

        if void_tag && children_html.is_empty() {
            if attrs_html.is_empty() {
                Ok(format!("<{tag} />"))
            } else {
                Ok(format!("<{tag} {attrs_html} />"))
            }
        } else if attrs_html.is_empty() {
            Ok(format!("<{tag}>{children_html}</{tag}>"))
        } else {
            Ok(format!("<{tag} {attrs_html}>{children_html}</{tag}>"))
        }
    }

    fn render_component_ref(
        &self,
        module_spec: &str,
        component: &str,
        props: &Value,
    ) -> Result<String> {
        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;

        if let Some(import_binding) = module.imports.get(component) {
            if import_binding.source == "react" {
                return Ok(String::new());
            }
            let target = self
                .resolve_import(module_spec, &import_binding.source)
                .ok_or_else(|| {
                    anyhow!(
                        "could not resolve import '{}' from '{}'",
                        import_binding.source,
                        module_spec
                    )
                })?;
            return self.render_export(&target, &import_binding.export_name, props);
        }

        self.render_local(module_spec, component, props)
    }

    fn read_attrs(
        &self,
        module_spec: &str,
        attrs: &[swc_ecma_ast::JSXAttrOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Vec<(String, Value)>> {
        use swc_ecma_ast::*;
        let mut out = Vec::new();
        for attr in attrs {
            match attr {
                JSXAttrOrSpread::SpreadElement(_) => {
                    return Err(anyhow!("spread attributes are not supported"));
                }
                JSXAttrOrSpread::JSXAttr(attr) => {
                    let name = match &attr.name {
                        JSXAttrName::Ident(ident) => ident.sym.to_string(),
                        _ => return Err(anyhow!("unsupported JSX attribute name")),
                    };
                    let value = match &attr.value {
                        None => Value::Bool(true),
                        Some(JSXAttrValue::Lit(lit)) => lit_to_value(lit),
                        Some(JSXAttrValue::JSXExprContainer(container)) => match &container.expr {
                            JSXExpr::Expr(expr) => self.eval_expr(module_spec, expr, env)?,
                            JSXExpr::JSXEmptyExpr(_) => Value::Null,
                        },
                        _ => Value::Null,
                    };
                    out.push((name, value));
                }
            }
        }
        Ok(out)
    }

    fn read_children_as_values(
        &self,
        module_spec: &str,
        children: &[swc_ecma_ast::JSXElementChild],
        env: &HashMap<String, Value>,
    ) -> Result<Vec<Value>> {
        use swc_ecma_ast::*;
        let mut out = Vec::new();
        for child in children {
            match child {
                JSXElementChild::JSXText(text) => {
                    if let Some(normalized) = normalize_jsx_text(text.value.as_ref()) {
                        out.push(Value::String(normalized));
                    }
                }
                JSXElementChild::JSXExprContainer(container) => match &container.expr {
                    JSXExpr::Expr(expr) => {
                        let value = self.eval_expr(module_spec, expr, env)?;
                        if !matches!(value, Value::Null | Value::Bool(false)) {
                            out.push(value);
                        }
                    }
                    JSXExpr::JSXEmptyExpr(_) => {}
                },
                JSXElementChild::JSXElement(element) => {
                    out.push(Value::String(self.eval_jsx_element(
                        module_spec,
                        element,
                        env,
                    )?));
                }
                JSXElementChild::JSXFragment(fragment) => {
                    out.push(Value::String(self.eval_jsx_fragment(
                        module_spec,
                        fragment,
                        env,
                    )?));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn render_children(
        &self,
        module_spec: &str,
        children: &[swc_ecma_ast::JSXElementChild],
        env: &HashMap<String, Value>,
        escape_expr_children: bool,
    ) -> Result<String> {
        use swc_ecma_ast::*;
        let mut html = String::new();
        for child in children {
            match child {
                JSXElementChild::JSXText(text) => {
                    if let Some(normalized) = normalize_jsx_text(text.value.as_ref()) {
                        html.push_str(&escape_html(&normalized));
                    }
                }
                JSXElementChild::JSXExprContainer(container) => match &container.expr {
                    JSXExpr::Expr(expr) => {
                        // Phase K: when the child expression is a bare
                        // slot-bound identifier, the rendered text node
                        // becomes a reactive binding site. Emit
                        // SetTextRef targeting the containing element
                        // (top of element_stack) so bakabox subscribes
                        // it to the slot store and re-applies on
                        // future SlotSet opcodes.
                        if let Some(slot_id) = phase_k_detect_slot_text_read(expr) {
                            if let Some(stable_id) = phase_k_top_element() {
                                phase_k_emit(Instruction::SetTextRef {
                                    stable_id: StableId(stable_id),
                                    slot_id,
                                });
                            }
                        }
                        let value = self.eval_expr(module_spec, expr, env)?;
                        if matches!(value, Value::Null | Value::Bool(false)) {
                            continue;
                        }
                        let text = value_to_string(&value);
                        if escape_expr_children {
                            html.push_str(&escape_html(&text));
                        } else {
                            html.push_str(&text);
                        }
                    }
                    JSXExpr::JSXEmptyExpr(_) => {}
                },
                JSXElementChild::JSXElement(element) => {
                    html.push_str(&self.eval_jsx_element(module_spec, element, env)?);
                }
                JSXElementChild::JSXFragment(fragment) => {
                    html.push_str(&self.eval_jsx_fragment(module_spec, fragment, env)?);
                }
                _ => {}
            }
        }
        Ok(html)
    }

    fn resolve_import(&self, current_module: &str, source: &str) -> Option<String> {
        if !source.starts_with('.') {
            return None;
        }

        let current_dir = Path::new(current_module)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let base = normalize_specifier(current_dir.join(source));
        for candidate in import_candidates(&base) {
            if self.modules.contains_key(&candidate) {
                return Some(candidate);
            }
        }

        if let Some(stripped) = source.strip_prefix("./components/") {
            let alt = normalize_specifier(PathBuf::from(stripped));
            for candidate in import_candidates(&alt) {
                if self.modules.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

pub fn render_from_components_dir(
    components_root: impl AsRef<Path>,
    entry_module: &str,
    props: &Value,
) -> Result<String> {
    let project = ComponentProject::load_from_dir(components_root)?;
    project.render_entry(entry_module, props)
}
