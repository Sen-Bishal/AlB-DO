//! Phase O.2 · `useSharedSlot` extractor.
//!
//! Walks a parsed component body and surfaces every call to
//! `useSharedSlot<T>("topic")`. Returns one [`SharedSlotBinding`]
//! per call in source order, mirroring the structural contract the
//! Phase K [`crate::transforms::hooks`] extractor established.
//!
//! ## Surface
//!
//! ```tsx
//! const messages = useSharedSlot<Message[]>("chat:room-42");   // a literal topic
//! const entries  = useSharedSlot(guestbook);                   // a whole collection
//! const rows     = useSharedSlot(messages.where({ room: params.id })); // one partition
//! ```
//!
//! The last two read collections imported from `albedo/forge`, the
//! compiler-generated binding module. `.where(…)` is a **compile-time
//! marker, not a runtime call** — nothing executes it; the extractor
//! lowers it to [`TopicSpec::Partition`] and the topic identity is
//! minted from `(collection, key)` at resolve time (PRISM § 3).
//!
//! That the author cannot spell the partition's topic string is the
//! point, not an inconvenience: a hand-written `` `room:${id}` ``
//! namespace admits two logically distinct partitions colliding on one
//! channel and cross-delivering their rows. Minting the identity makes
//! that unexpressible rather than merely checked.
//!
//! Unlike `useState`, the hook returns a **single read binding** — no
//! setter pair. Writes to a shared slot are authored server-side via
//! [`crate::runtime::BroadcastRegistry::write_topic`] from an action
//! handler. This matches the framework's broader pattern: events
//! travel client→server, writes happen server-side, the bakabox
//! client just consumes `SlotSet` opcodes off the WT patches lane.
//!
//! ## Rejection rules
//!
//! - `useSharedSlot` inside a conditional / loop body → would change
//!   the binding count across renders; bakabox cannot align slots
//!   stably so we refuse at compile time.
//! - Missing argument → no topic to subscribe to.
//! - An argument that is none of the three forms above → the topic has
//!   to be derivable *without running the component*, because the
//!   subscribe path (`RouteAuthority::authorize_route`) resolves the
//!   same binding from the route path alone. A free expression also
//!   makes the wire slot id non-deterministic across builds.
//! - A partition key outside the closed vocabulary (`params.<name>`
//!   today) → same reason, plus: this value reaches both a topic
//!   namespace and a SQL parameter. `user.id` is recognised and
//!   rejected with a message naming item 5, rather than lumped in with
//!   genuine nonsense.
//! - Non-identifier destructure pattern → no name to bind the read
//!   value to. (`const { foo } = useSharedSlot(...)` is unsupported;
//!   only `const x = useSharedSlot(...)` matches.)
//!
//! ## Import binding contract
//!
//! Only `useSharedSlot` symbols imported from `albedo` (or the
//! ambient global type surface emitted by Phase M.3) are recognised.
//! A user-defined function literally named `useSharedSlot` shadowing
//! the framework export is skipped — this matches the
//! `extract_use_state_hooks` rule that pins identifiers to their
//! `react` import.

use crate::runtime::eval::{ComponentFunction, ImportBinding};
use std::collections::HashMap;
use swc_common::DUMMY_SP;
use swc_ecma_ast::{
    BlockStmtOrExpr, CallExpr, Callee, Decl, Expr, ExprOrSpread, ExprStmt, ForStmt, Ident, IfStmt,
    ImportSpecifier, Lit, MemberProp, Module, ModuleDecl, ModuleExportName, ModuleItem, Pat, Prop,
    PropName, PropOrSpread, Stmt, Str, VarDeclarator,
};
use swc_ecma_visit::{VisitMut, VisitMutWith};

/// Where a `useSharedSlot` binding's runtime key comes from.
///
/// A closed vocabulary, deliberately. The subscribe path resolves the same
/// binding without a component render (`RouteAuthority::authorize_route` has
/// only the path and the lane identity), so anything it cannot evaluate there
/// is refused here — and a key that reaches both a topic namespace and a SQL
/// parameter is not a place to accept arbitrary expressions.
///
/// `user.id` is **not** a variant yet: it is recognised and rejected with a
/// message naming item 5 (auth), and becomes `Identity` when a session is
/// actually in scope. See [`SharedSlotExtractError::IdentityKeyNotYetSupported`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// A route parameter: `params.id` on `/room/[id]`.
    Param(String),
}

/// Which broadcast topic a `useSharedSlot` binding reads.
///
/// PRISM § 3: the author never writes the topic string for a partitioned
/// collection. The compiler mints one canonical identity per
/// `(collection, key)`, which is what makes two logically distinct partitions
/// aliasing onto one channel *unexpressible* rather than merely guarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicSpec {
    /// A topic known in full at compile time — `useSharedSlot("guestbook")`, or
    /// a whole unpartitioned collection, `useSharedSlot(guestbook)`.
    Static(String),
    /// One partition of a collection:
    /// `useSharedSlot(messages.where({ room: params.id }))`.
    ///
    /// Has **no compile-time topic string** — resolving it needs runtime keys —
    /// which is exactly why [`SharedSlotBinding::static_topic`] returns `None`
    /// here and every caller that pre-registers topics at boot skips it.
    Partition {
        /// The declared collection name (the `forge` block key), taken from the
        /// *import's export name*, so `import { messages as msgs }` still
        /// resolves to `messages`.
        collection: String,
        /// The column named by `.where({ <column>: … })`. Checked against the
        /// collection's declared `partitionBy` at build time — this extractor
        /// has no schema, so it records what was written and lets the boot check
        /// compare.
        column: String,
        key: KeySource,
    },
}

/// One `useSharedSlot` call extracted from a component body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSlotBinding {
    /// Position among all `useSharedSlot` calls in the component body,
    /// in source order. Stable across recompilations of the same
    /// source. Use this to pair extractor output with later passes.
    pub hook_idx: usize,
    /// The local name the read binding is assigned to.
    pub binding_name: String,
    /// What this binding reads. The wire slot id is derived from the *resolved*
    /// topic string via [`crate::runtime::broadcast_slot_id`] —
    /// `"broadcast::{topic}"` hashed FNV-1a-32. The extractor does not compute
    /// the slot id; that's the broadcast registry's responsibility so the same
    /// hash function ships everywhere.
    pub spec: TopicSpec,
}

impl SharedSlotBinding {
    /// The topic string when it is known at compile time, `None` for a
    /// partitioned binding.
    ///
    /// Every caller that pre-registers topics at startup or seeds a render from
    /// the registry wants this: a partition cannot be named before its key
    /// exists, so those paths must skip it rather than invent one.
    #[must_use]
    pub fn static_topic(&self) -> Option<&str> {
        match &self.spec {
            TopicSpec::Static(topic) => Some(topic.as_str()),
            TopicSpec::Partition { .. } => None,
        }
    }
}

/// Failure modes refused at compile time. Surfaced verbatim through
/// [`crate::runtime::compiled::CompiledProject::wrap`] so misuse
/// stops the build instead of slipping into runtime.
#[derive(Debug, PartialEq, Eq)]
pub enum SharedSlotExtractError {
    HookInsideConditional { hook_idx_so_far: usize, location: String },
    MissingTopicArgument { binding_name: Option<String> },
    NonStringLiteralTopic { binding_name: Option<String> },
    UnsupportedDestructurePattern,
    /// `.where(…)` was passed something other than a single-property object
    /// literal naming the partition column.
    UnsupportedWhereShape { binding_name: Option<String> },
    /// The partition key is an expression the subscribe path cannot reproduce.
    PartitionKeyUnsupported {
        binding_name: Option<String>,
        found: String,
    },
    /// The partition key is `user.…`, which is real and scheduled — item 5.
    /// Distinguished from [`Self::PartitionKeyUnsupported`] so the author learns
    /// the feature is *coming*, not that they wrote nonsense.
    IdentityKeyNotYetSupported { binding_name: Option<String> },
}

impl std::fmt::Display for SharedSlotExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HookInsideConditional { hook_idx_so_far, location } => write!(
                f,
                "useSharedSlot invoked inside a conditional ({location}); hooks must run \
                 unconditionally (would have been call #{hook_idx_so_far} in source order)",
            ),
            Self::MissingTopicArgument { binding_name } => write!(
                f,
                "useSharedSlot{} requires a string-literal topic argument",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::NonStringLiteralTopic { binding_name } => write!(
                f,
                "useSharedSlot{} takes a string-literal topic, a collection imported from \
                 'albedo/forge', or one partition of one (`messages.where({{ room: params.id }})`) \
                 — the topic has to be derivable without running the component, because the \
                 subscribe path resolves it from the route alone",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::UnsupportedDestructurePattern => f.write_str(
                "useSharedSlot must bind to a single identifier (e.g. `const x = useSharedSlot(\"topic\")`)",
            ),
            Self::UnsupportedWhereShape { binding_name } => write!(
                f,
                "useSharedSlot{}: .where() takes one object literal naming the partition column, \
                 e.g. `messages.where({{ room: params.id }})`",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::PartitionKeyUnsupported {
                binding_name,
                found,
            } => write!(
                f,
                "useSharedSlot{}: a partition key must be a route parameter (`params.id`); \
                 found `{found}`. The subscribe path resolves this binding from the route path \
                 alone, with no component render to evaluate an expression in",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::IdentityKeyNotYetSupported { binding_name } => write!(
                f,
                "useSharedSlot{}: per-user partitions (`user.id`) need sessions, which land with \
                 auth — TODO #1 item 5. Route parameters (`params.id`) work today",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
        }
    }
}

impl std::error::Error for SharedSlotExtractError {}

/// Extract every `useSharedSlot` call from a component body in
/// source-traversal order.
///
/// `imports` is the function's containing module's import map; only
/// identifiers that resolve back to an `albedo` import named
/// `useSharedSlot` are recognised. Anything else is treated as user
/// code and ignored.
pub fn extract_shared_slot_hooks(
    function: &ComponentFunction,
    imports: &HashMap<String, ImportBinding>,
) -> Result<Vec<SharedSlotBinding>, SharedSlotExtractError> {
    let mut out = Vec::new();
    for stmt in &function.body_stmts {
        visit_stmt_top_level(stmt, imports, &mut out)?;
    }
    Ok(out)
}

fn visit_stmt_top_level(
    stmt: &Stmt,
    imports: &HashMap<String, ImportBinding>,
    out: &mut Vec<SharedSlotBinding>,
) -> Result<(), SharedSlotExtractError> {
    match stmt {
        Stmt::Decl(Decl::Var(var)) => {
            for decl in &var.decls {
                try_extract_from_var_declarator(decl, imports, out)?;
            }
            Ok(())
        }
        Stmt::If(IfStmt { cons, alt, .. }) => {
            check_no_shared_slot_calls_in_stmt(cons, imports, out.len())?;
            if let Some(alt) = alt {
                check_no_shared_slot_calls_in_stmt(alt, imports, out.len())?;
            }
            Ok(())
        }
        Stmt::For(ForStmt { body, .. }) => {
            check_no_shared_slot_calls_in_stmt(body, imports, out.len())
        }
        Stmt::While(node) => check_no_shared_slot_calls_in_stmt(&node.body, imports, out.len()),
        Stmt::DoWhile(node) => check_no_shared_slot_calls_in_stmt(&node.body, imports, out.len()),
        Stmt::Try(node) => {
            for inner in &node.block.stmts {
                check_no_shared_slot_calls_in_stmt(inner, imports, out.len())?;
            }
            Ok(())
        }
        Stmt::Block(block) => {
            for inner in &block.stmts {
                visit_stmt_top_level(inner, imports, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn try_extract_from_var_declarator(
    decl: &VarDeclarator,
    imports: &HashMap<String, ImportBinding>,
    out: &mut Vec<SharedSlotBinding>,
) -> Result<(), SharedSlotExtractError> {
    let Some(init) = &decl.init else { return Ok(()) };
    let Expr::Call(call) = init.as_ref() else { return Ok(()) };
    if !is_use_shared_slot_call(call, imports) {
        return Ok(());
    }

    let binding_name = match &decl.name {
        Pat::Ident(ident) => Some(ident.id.sym.to_string()),
        _ => None,
    };

    let spec = extract_topic_spec(call, &binding_name, imports)?;
    let binding_name = binding_name.ok_or(SharedSlotExtractError::UnsupportedDestructurePattern)?;

    out.push(SharedSlotBinding {
        hook_idx: out.len(),
        binding_name,
        spec,
    });
    Ok(())
}

/// The module specifier the compiler-generated collection bindings come from.
/// Pinned the same way `useSharedSlot` itself is pinned to `albedo`: a local
/// variable that happens to share a collection's name is user code, not a
/// collection reference.
const FORGE_BINDINGS_MODULE: &str = "albedo/forge";

fn extract_topic_spec(
    call: &CallExpr,
    binding_name: &Option<String>,
    imports: &HashMap<String, ImportBinding>,
) -> Result<TopicSpec, SharedSlotExtractError> {
    let first = call.args.first().ok_or_else(|| {
        SharedSlotExtractError::MissingTopicArgument { binding_name: binding_name.clone() }
    })?;
    match unwrap_parens(&first.expr) {
        // `useSharedSlot("guestbook")` — the original surface, unchanged.
        Expr::Lit(Lit::Str(s)) => Ok(TopicSpec::Static(s.value.to_string())),
        // `useSharedSlot(guestbook)` — a whole collection. Same topic the string
        // form names, reached without spelling it.
        Expr::Ident(ident) => collection_for_ident(ident.sym.as_ref(), imports)
            .map(TopicSpec::Static)
            .ok_or_else(|| SharedSlotExtractError::NonStringLiteralTopic {
                binding_name: binding_name.clone(),
            }),
        // `useSharedSlot(messages.where({ room: params.id }))` — one partition.
        Expr::Call(inner) => extract_partition(inner, binding_name, imports),
        _ => Err(SharedSlotExtractError::NonStringLiteralTopic {
            binding_name: binding_name.clone(),
        }),
    }
}

/// The declared collection name behind a local identifier, when it is bound by
/// an import from [`FORGE_BINDINGS_MODULE`].
///
/// Returns the import's **export name**, not the local alias, so
/// `import { messages as msgs } from "albedo/forge"` still resolves to the
/// `forge` block's `messages`.
fn collection_for_ident(local: &str, imports: &HashMap<String, ImportBinding>) -> Option<String> {
    imports
        .get(local)
        .filter(|binding| binding.source == FORGE_BINDINGS_MODULE)
        .map(|binding| binding.export_name.clone())
}

/// Lower `<collection>.where({ <column>: <key> })`.
fn extract_partition(
    call: &CallExpr,
    binding_name: &Option<String>,
    imports: &HashMap<String, ImportBinding>,
) -> Result<TopicSpec, SharedSlotExtractError> {
    let unsupported = || SharedSlotExtractError::NonStringLiteralTopic {
        binding_name: binding_name.clone(),
    };

    // Callee must be `<ident>.where`, and `<ident>` must be a collection.
    let Callee::Expr(callee) = &call.callee else { return Err(unsupported()) };
    let Expr::Member(member) = unwrap_parens(callee) else { return Err(unsupported()) };
    let MemberProp::Ident(method) = &member.prop else { return Err(unsupported()) };
    if method.sym.as_ref() != "where" {
        return Err(unsupported());
    }
    let Expr::Ident(receiver) = unwrap_parens(&member.obj) else { return Err(unsupported()) };
    let collection = collection_for_ident(receiver.sym.as_ref(), imports).ok_or_else(unsupported)?;

    // Exactly one argument, an object literal with exactly one `key: value`
    // property. More than one partition column is a composite key, which is a
    // later rung — refusing it now keeps the identity minting unambiguous.
    let [arg] = call.args.as_slice() else {
        return Err(SharedSlotExtractError::UnsupportedWhereShape {
            binding_name: binding_name.clone(),
        });
    };
    let Expr::Object(object) = unwrap_parens(&arg.expr) else {
        return Err(SharedSlotExtractError::UnsupportedWhereShape {
            binding_name: binding_name.clone(),
        });
    };
    let [PropOrSpread::Prop(prop)] = object.props.as_slice() else {
        return Err(SharedSlotExtractError::UnsupportedWhereShape {
            binding_name: binding_name.clone(),
        });
    };
    let Prop::KeyValue(entry) = prop.as_ref() else {
        return Err(SharedSlotExtractError::UnsupportedWhereShape {
            binding_name: binding_name.clone(),
        });
    };
    let column = match &entry.key {
        PropName::Ident(ident) => ident.sym.to_string(),
        PropName::Str(s) => s.value.to_string(),
        _ => {
            return Err(SharedSlotExtractError::UnsupportedWhereShape {
                binding_name: binding_name.clone(),
            })
        }
    };

    Ok(TopicSpec::Partition {
        collection,
        column,
        key: extract_key_source(&entry.value, binding_name)?,
    })
}

/// Lower the value side of `.where({ column: <here> })`.
fn extract_key_source(
    expr: &Expr,
    binding_name: &Option<String>,
) -> Result<KeySource, SharedSlotExtractError> {
    if let Expr::Member(member) = unwrap_parens(expr) {
        if let (Expr::Ident(base), MemberProp::Ident(field)) =
            (unwrap_parens(&member.obj), &member.prop)
        {
            match base.sym.as_ref() {
                "params" => return Ok(KeySource::Param(field.sym.to_string())),
                // Recognised on purpose: the author is asking for the right
                // thing, it just isn't wired yet.
                "user" | "session" => {
                    return Err(SharedSlotExtractError::IdentityKeyNotYetSupported {
                        binding_name: binding_name.clone(),
                    })
                }
                _ => {}
            }
        }
    }
    Err(SharedSlotExtractError::PartitionKeyUnsupported {
        binding_name: binding_name.clone(),
        found: describe_expr(expr),
    })
}

/// A short, non-exhaustive label for an expression, for error messages only.
fn describe_expr(expr: &Expr) -> String {
    match unwrap_parens(expr) {
        Expr::Ident(ident) => ident.sym.to_string(),
        Expr::Lit(Lit::Str(s)) => format!("\"{}\"", s.value),
        Expr::Lit(_) => "a literal".to_string(),
        Expr::Member(member) => match (unwrap_parens(&member.obj), &member.prop) {
            (Expr::Ident(base), MemberProp::Ident(field)) => format!("{}.{}", base.sym, field.sym),
            _ => "a property access".to_string(),
        },
        Expr::Call(_) => "a function call".to_string(),
        Expr::Tpl(_) => "a template literal".to_string(),
        Expr::Bin(_) => "an expression".to_string(),
        _ => "an unsupported expression".to_string(),
    }
}

// ── QuickJS call-site rewrite ────────────────────────────────────────────
//
// The pure-Rust interpreter never evaluates a `useSharedSlot(…)` call: it
// resolves the binding by NAME out of the scope map the render seeded
// (`eval/core.rs`, `phase_k_shared_slot_for_value`). QuickJS does the opposite —
// its shim evaluates the argument and stringifies it
// (`quickjs_engine.rs`, `globalThis.useSharedSlot = function(topic) { … }`).
//
// That asymmetry is fine while every topic is a string literal and becomes a
// `ReferenceError` the moment the argument is a collection reference, because
// `albedo/forge` has no module record to bind — by design; it is a types-only
// module (see `is_framework_runtime_import`).
//
// So the transpile folds the argument down to something the shim can evaluate
// without the collection existing at runtime, and the two engines stop
// disagreeing:
//
//   useSharedSlot("guestbook")                      → untouched
//   useSharedSlot(guestbook)                        → useSharedSlot("guestbook")
//   useSharedSlot(m.where({ room: params.id }))     → useSharedSlot(__albedo_topic("rows"))
//
// The literal form is deliberately left byte-identical: every app that exists
// today takes that branch, so this pass cannot change their output.
//
// Resolution stays entirely in Rust. `__albedo_topic(binding)` reads
// `host.topics[binding]`, which the render path fills after resolving the spec
// against the route's params — the same single resolver the subscribe path
// uses. Nothing mints a topic string in JS.

/// Fold `useSharedSlot`'s topic argument into a form the QuickJS shim can
/// evaluate. See the module-level note above.
///
/// Self-contained: reads the module's own imports, so the transpile caller does
/// not thread component analysis in. Mirrors
/// [`try_extract_from_var_declarator`]'s shape exactly — same declarator
/// pattern, same import pins — so the extractor and this rewrite cannot
/// disagree about which calls are shared slots.
pub fn rewrite_shared_slot_topic_args(module: &mut Module) {
    let Some(hook_local) = local_name_for(module, "albedo", "useSharedSlot") else {
        return;
    };
    let collections = forge_collection_locals(module);
    module.visit_mut_with(&mut TopicArgRewriter { hook_local, collections });
}

/// `local -> export_name` for every identifier imported from
/// [`FORGE_BINDINGS_MODULE`].
fn forge_collection_locals(module: &Module) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for item in &module.body {
        let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item else {
            continue;
        };
        if import.src.value.as_ref() != FORGE_BINDINGS_MODULE {
            continue;
        }
        for spec in &import.specifiers {
            let ImportSpecifier::Named(named) = spec else {
                continue;
            };
            let exported = match &named.imported {
                Some(ModuleExportName::Ident(i)) => i.sym.to_string(),
                Some(ModuleExportName::Str(s)) => s.value.to_string(),
                None => named.local.sym.to_string(),
            };
            out.insert(named.local.sym.to_string(), exported);
        }
    }
    out
}

/// The local identifier `export_name` is imported as from `source`, if any.
/// Mirrors the extractor's import-binding rule so a user symbol that merely
/// shares the name is never mistaken for the framework's.
fn local_name_for(module: &Module, source: &str, export_name: &str) -> Option<String> {
    for item in &module.body {
        let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item else {
            continue;
        };
        if import.src.value.as_ref() != source {
            continue;
        }
        for spec in &import.specifiers {
            let ImportSpecifier::Named(named) = spec else {
                continue;
            };
            let imported = match &named.imported {
                Some(ModuleExportName::Ident(i)) => i.sym.to_string(),
                Some(ModuleExportName::Str(s)) => s.value.to_string(),
                None => named.local.sym.to_string(),
            };
            if imported == export_name {
                return Some(named.local.sym.to_string());
            }
        }
    }
    None
}

struct TopicArgRewriter {
    hook_local: String,
    collections: HashMap<String, String>,
}

impl TopicArgRewriter {
    /// The replacement for this call's topic argument, or `None` to leave it be.
    fn folded_arg(&self, call: &CallExpr, binding: &str) -> Option<Expr> {
        let arg = unwrap_parens(&call.args.first()?.expr);
        match arg {
            // Already evaluable — and the shape every existing app uses.
            Expr::Lit(Lit::Str(_)) => None,
            // A whole collection: its topic is known now, so fold it to the
            // literal rather than paying a runtime lookup for a constant.
            Expr::Ident(ident) => self
                .collections
                .get(ident.sym.as_ref())
                .map(|collection| string_expr(collection)),
            // A partition: only the render knows the key, so defer to the host.
            Expr::Call(inner) if self.is_where_on_collection(inner) => {
                Some(topic_lookup_expr(binding))
            }
            _ => None,
        }
    }

    fn is_where_on_collection(&self, call: &CallExpr) -> bool {
        let Callee::Expr(callee) = &call.callee else { return false };
        let Expr::Member(member) = unwrap_parens(callee) else { return false };
        let MemberProp::Ident(method) = &member.prop else { return false };
        if method.sym.as_ref() != "where" {
            return false;
        }
        matches!(unwrap_parens(&member.obj), Expr::Ident(receiver)
            if self.collections.contains_key(receiver.sym.as_ref()))
    }
}

impl VisitMut for TopicArgRewriter {
    fn visit_mut_var_declarator(&mut self, decl: &mut VarDeclarator) {
        decl.visit_mut_children_with(self);

        let Pat::Ident(binding) = &decl.name else { return };
        let binding = binding.id.sym.to_string();
        let Some(init) = &mut decl.init else { return };
        let Expr::Call(call) = init.as_mut() else { return };
        if !callee_is_ident(call, &self.hook_local) {
            return;
        }
        if let Some(folded) = self.folded_arg(call, &binding) {
            call.args[0] = ExprOrSpread { spread: None, expr: Box::new(folded) };
        }
    }
}

fn callee_is_ident(call: &CallExpr, name: &str) -> bool {
    let Callee::Expr(callee) = &call.callee else { return false };
    matches!(callee.as_ref(), Expr::Ident(ident) if ident.sym.as_ref() == name)
}

fn string_expr(value: &str) -> Expr {
    Expr::Lit(Lit::Str(Str { span: DUMMY_SP, value: value.into(), raw: None }))
}

/// `__albedo_topic("<binding>")` — the host lookup for a topic only the render
/// can resolve.
fn topic_lookup_expr(binding: &str) -> Expr {
    Expr::Call(CallExpr {
        span: DUMMY_SP,
        callee: Callee::Expr(Box::new(Expr::Ident(Ident::new_no_ctxt(
            TOPIC_LOOKUP_FN.into(),
            DUMMY_SP,
        )))),
        args: vec![ExprOrSpread { spread: None, expr: Box::new(string_expr(binding)) }],
        type_args: None,
        ctxt: Default::default(),
    })
}

/// The global the transpile emits for a deferred topic lookup. Installed
/// alongside the other hook shims.
pub const TOPIC_LOOKUP_FN: &str = "__albedo_topic";

/// Peel `(expr)` wrappers so `useSharedSlot(("topic"))` still
/// extracts cleanly. SWC also surfaces `TsAs` / `TsSatisfies` wrappers
/// when the user writes `useSharedSlot("t" as const)`; those pass
/// through too.
fn unwrap_parens(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(p) => unwrap_parens(&p.expr),
        Expr::TsAs(ts_as) => unwrap_parens(&ts_as.expr),
        Expr::TsSatisfies(ts_sat) => unwrap_parens(&ts_sat.expr),
        Expr::TsConstAssertion(ts_const) => unwrap_parens(&ts_const.expr),
        other => other,
    }
}

fn is_use_shared_slot_call(call: &CallExpr, imports: &HashMap<String, ImportBinding>) -> bool {
    let Callee::Expr(callee) = &call.callee else { return false };
    let Expr::Ident(ident) = callee.as_ref() else { return false };
    let name = ident.sym.to_string();
    if name != "useSharedSlot" {
        return false;
    }
    imports
        .get(&name)
        .map(|b| b.source == "albedo" && b.export_name == "useSharedSlot")
        .unwrap_or(false)
}

fn check_no_shared_slot_calls_in_stmt(
    stmt: &Stmt,
    imports: &HashMap<String, ImportBinding>,
    hook_idx_so_far: usize,
) -> Result<(), SharedSlotExtractError> {
    let location = match stmt {
        Stmt::If(_) => "inside if-statement body",
        Stmt::For(_) => "inside for-loop body",
        Stmt::While(_) => "inside while-loop body",
        Stmt::DoWhile(_) => "inside do-while-loop body",
        Stmt::Block(_) => "inside nested block",
        _ => "inside conditional path",
    };
    if stmt_contains_shared_slot_call(stmt, imports) {
        return Err(SharedSlotExtractError::HookInsideConditional {
            hook_idx_so_far,
            location: location.to_string(),
        });
    }
    Ok(())
}

fn stmt_contains_shared_slot_call(stmt: &Stmt, imports: &HashMap<String, ImportBinding>) -> bool {
    match stmt {
        Stmt::Decl(Decl::Var(var)) => var.decls.iter().any(|d| {
            d.init
                .as_ref()
                .map(|e| expr_contains_shared_slot_call(e, imports))
                .unwrap_or(false)
        }),
        Stmt::Expr(ExprStmt { expr, .. }) => expr_contains_shared_slot_call(expr, imports),
        Stmt::Block(block) => block
            .stmts
            .iter()
            .any(|s| stmt_contains_shared_slot_call(s, imports)),
        Stmt::If(IfStmt { cons, alt, .. }) => {
            stmt_contains_shared_slot_call(cons, imports)
                || alt
                    .as_ref()
                    .map(|a| stmt_contains_shared_slot_call(a, imports))
                    .unwrap_or(false)
        }
        Stmt::For(ForStmt { body, .. }) => stmt_contains_shared_slot_call(body, imports),
        Stmt::While(node) => stmt_contains_shared_slot_call(&node.body, imports),
        Stmt::DoWhile(node) => stmt_contains_shared_slot_call(&node.body, imports),
        Stmt::Return(node) => node
            .arg
            .as_ref()
            .map(|e| expr_contains_shared_slot_call(e, imports))
            .unwrap_or(false),
        _ => false,
    }
}

fn expr_contains_shared_slot_call(expr: &Expr, imports: &HashMap<String, ImportBinding>) -> bool {
    match expr {
        Expr::Call(call) => {
            if is_use_shared_slot_call(call, imports) {
                return true;
            }
            call.args
                .iter()
                .any(|a| expr_contains_shared_slot_call(&a.expr, imports))
        }
        Expr::Bin(b) => {
            expr_contains_shared_slot_call(&b.left, imports)
                || expr_contains_shared_slot_call(&b.right, imports)
        }
        Expr::Cond(c) => {
            expr_contains_shared_slot_call(&c.test, imports)
                || expr_contains_shared_slot_call(&c.cons, imports)
                || expr_contains_shared_slot_call(&c.alt, imports)
        }
        Expr::Paren(p) => expr_contains_shared_slot_call(&p.expr, imports),
        Expr::Arrow(arrow) => match &*arrow.body {
            BlockStmtOrExpr::BlockStmt(block) => block
                .stmts
                .iter()
                .any(|s| stmt_contains_shared_slot_call(s, imports)),
            BlockStmtOrExpr::Expr(e) => expr_contains_shared_slot_call(e, imports),
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::eval::expr::parse_module;
    use crate::runtime::eval::ParsedModule;
    use std::path::Path;

    /// Compile a TSX fragment to a `ParsedModule` and return the
    /// component function named `Component`.
    fn parse(source: &str) -> ParsedModule {
        parse_module(source, Path::new("test_module.tsx")).expect("parse")
    }

    fn function_named<'a>(module: &'a ParsedModule, name: &str) -> &'a ComponentFunction {
        module.functions.get(name).expect("function present")
    }

    fn extract_or_panic(source: &str) -> Vec<SharedSlotBinding> {
        let parsed = parse(source);
        let function = function_named(&parsed, "Component");
        extract_shared_slot_hooks(function, &parsed.imports).expect("extraction")
    }

    fn extract_err(source: &str) -> SharedSlotExtractError {
        let parsed = parse(source);
        let function = function_named(&parsed, "Component");
        extract_shared_slot_hooks(function, &parsed.imports).expect_err("expected extraction error")
    }

    #[test]
    fn extracts_single_call_with_topic_and_binding_name() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const messages = useSharedSlot("chat:room-42");
                return <ul>{messages}</ul>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].hook_idx, 0);
        assert_eq!(bindings[0].binding_name, "messages");
        assert_eq!(bindings[0].static_topic(), Some("chat:room-42"));
    }

    /// A whole collection reached through its generated binding is the same
    /// topic the string form names — the author just didn't have to spell it.
    #[test]
    fn a_collection_reference_is_a_static_topic() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { guestbook } from "albedo/forge";
            export default function Component() {
                const entries = useSharedSlot(guestbook);
                return <ul>{entries}</ul>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].static_topic(), Some("guestbook"));
    }

    /// The headline P0 surface. `.where` lowers to a partition, and the
    /// partition has **no** compile-time topic — that `None` is what keeps it
    /// out of boot pre-registration and out of the render seed.
    #[test]
    fn a_where_clause_lowers_to_a_partition_with_no_static_topic() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { messages } from "albedo/forge";
            export default function Component({ params }) {
                const rows = useSharedSlot(messages.where({ room: params.id }));
                return <ul>{rows}</ul>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].binding_name, "rows");
        assert_eq!(
            bindings[0].spec,
            TopicSpec::Partition {
                collection: "messages".to_string(),
                column: "room".to_string(),
                key: KeySource::Param("id".to_string()),
            }
        );
        assert_eq!(
            bindings[0].static_topic(),
            None,
            "a partition cannot be named before its key exists; every \
             pre-registration path keys off this being None"
        );
    }

    /// The collection is the import's EXPORT name, not the local alias —
    /// otherwise `import { messages as msgs }` would mint a topic for a
    /// collection that does not exist.
    #[test]
    fn an_aliased_collection_import_resolves_to_its_declared_name() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { messages as msgs } from "albedo/forge";
            export default function Component({ params }) {
                const rows = useSharedSlot(msgs.where({ room: params.id }));
                return <ul>{rows}</ul>;
            }
            "#,
        );
        match &bindings[0].spec {
            TopicSpec::Partition { collection, .. } => assert_eq!(collection, "messages"),
            other => panic!("expected a partition, got {other:?}"),
        }
    }

    /// Same discipline as `useSharedSlot` itself: a local named like a
    /// collection is user code. Without the import pin, any identifier would
    /// silently become a topic.
    #[test]
    fn a_collection_name_not_imported_from_forge_is_refused() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            import { messages } from "./my-own-module";
            export default function Component({ params }) {
                const rows = useSharedSlot(messages.where({ room: params.id }));
                return <ul>{rows}</ul>;
            }
            "#,
        );
        assert!(
            matches!(err, SharedSlotExtractError::NonStringLiteralTopic { .. }),
            "got {err:?}"
        );
    }

    /// `user.id` is the right idea at the wrong time. It must not be lumped in
    /// with genuine nonsense — the author should learn it is scheduled.
    #[test]
    fn a_user_partition_key_names_item_5_rather_than_failing_generically() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            import { todos } from "albedo/forge";
            export default function Component({ user }) {
                const rows = useSharedSlot(todos.where({ owner: user.id }));
                return <ul>{rows}</ul>;
            }
            "#,
        );
        assert!(
            matches!(err, SharedSlotExtractError::IdentityKeyNotYetSupported { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("item 5"),
            "the message must name the item that unblocks it: {err}"
        );
    }

    /// A computed key is exactly what the subscribe path cannot reproduce: it
    /// has no component render to evaluate the expression in.
    #[test]
    fn a_computed_partition_key_is_refused_and_quoted_back() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            import { messages } from "albedo/forge";
            export default function Component({ roomId }) {
                const rows = useSharedSlot(messages.where({ room: roomId }));
                return <ul>{rows}</ul>;
            }
            "#,
        );
        match &err {
            SharedSlotExtractError::PartitionKeyUnsupported { found, .. } => {
                assert_eq!(found, "roomId")
            }
            other => panic!("expected PartitionKeyUnsupported, got {other:?}"),
        }
    }

    /// Two partition columns is a composite key — a later rung. Refusing it now
    /// keeps identity minting unambiguous rather than half-defined.
    #[test]
    fn a_multi_column_where_is_refused() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            import { messages } from "albedo/forge";
            export default function Component({ params }) {
                const rows = useSharedSlot(messages.where({ room: params.id, org: params.org }));
                return <ul>{rows}</ul>;
            }
            "#,
        );
        assert!(
            matches!(err, SharedSlotExtractError::UnsupportedWhereShape { .. }),
            "got {err:?}"
        );
    }

    /// The whole point of P0: the legacy string form is untouched, so nothing
    /// observable changes until P3 wires resolution.
    #[test]
    fn the_string_form_still_lowers_to_the_same_static_topic() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const entries = useSharedSlot("guestbook");
                return <ul>{entries}</ul>;
            }
            "#,
        );
        assert_eq!(bindings[0].spec, TopicSpec::Static("guestbook".to_string()));
    }

    // ── the QuickJS topic-argument fold ──────────────────────────────────

    mod fold {
        use super::super::{rewrite_shared_slot_topic_args, TOPIC_LOOKUP_FN};
        use swc_common::{sync::Lrc, FileName, SourceMap};
        use swc_ecma_ast::{
            Callee, Decl, Expr, Lit, Module, ModuleItem, Pat, Stmt,
        };
        use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
        use swc_ecma_visit::{Visit, VisitWith};

        fn parse_tsx(source: &str) -> Module {
            let cm: Lrc<SourceMap> = Default::default();
            let fm =
                cm.new_source_file(FileName::Custom("test.tsx".into()).into(), source.to_string());
            let mut parser = Parser::new(
                Syntax::Typescript(TsSyntax { tsx: true, ..Default::default() }),
                StringInput::from(&*fm),
                None,
            );
            parser.parse_module().expect("tsx parses")
        }

        /// The folded topic argument of the first `useSharedSlot` declaration,
        /// rendered as a comparable label.
        fn folded_arg(source: &str) -> String {
            let mut module = parse_tsx(source);
            rewrite_shared_slot_topic_args(&mut module);

            struct Find(Option<String>);
            impl Visit for Find {
                fn visit_var_declarator(&mut self, decl: &swc_ecma_ast::VarDeclarator) {
                    if self.0.is_some() {
                        return;
                    }
                    let (Pat::Ident(_), Some(init)) = (&decl.name, &decl.init) else {
                        return;
                    };
                    let Expr::Call(call) = init.as_ref() else { return };
                    let Callee::Expr(callee) = &call.callee else { return };
                    let Expr::Ident(name) = callee.as_ref() else { return };
                    if name.sym.as_ref() != "useSharedSlot" {
                        return;
                    }
                    self.0 = Some(match call.args.first().map(|a| a.expr.as_ref()) {
                        Some(Expr::Lit(Lit::Str(s))) => format!("\"{}\"", s.value),
                        Some(Expr::Ident(i)) => i.sym.to_string(),
                        Some(Expr::Call(inner)) => {
                            let label = match &inner.callee {
                                Callee::Expr(c) => match c.as_ref() {
                                    Expr::Ident(i) => i.sym.to_string(),
                                    Expr::Member(_) => "<member call>".to_string(),
                                    _ => "<call>".to_string(),
                                },
                                _ => "<call>".to_string(),
                            };
                            match inner.args.first().map(|a| a.expr.as_ref()) {
                                Some(Expr::Lit(Lit::Str(s))) => format!("{label}(\"{}\")", s.value),
                                _ => format!("{label}(…)"),
                            }
                        }
                        _ => "<none>".to_string(),
                    });
                }
            }
            // `visit_var_declarator` is not reached by the default walk for
            // statements nested in a function body unless we walk children.
            let mut find = Find(None);
            for item in &module.body {
                if let ModuleItem::Stmt(Stmt::Decl(Decl::Fn(_))) = item {}
                item.visit_with(&mut find);
            }
            find.0.expect("a useSharedSlot declaration")
        }

        const HOOK: &str = "import { useSharedSlot } from \"albedo\";";
        const FORGE: &str = "import { messages, guestbook } from \"albedo/forge\";";

        /// The branch every app that exists today takes. It must come out
        /// byte-identical, which is what makes this pass safe to ship dark.
        #[test]
        fn a_string_literal_topic_is_left_untouched() {
            let out = folded_arg(&format!(
                "{HOOK} export default function C() {{ \
                 const rows = useSharedSlot(\"guestbook\"); return <ul>{{rows}}</ul>; }}"
            ));
            assert_eq!(out, "\"guestbook\"");
        }

        /// A whole collection's topic is a compile-time constant, so it folds to
        /// the literal rather than paying a host lookup for something known.
        #[test]
        fn a_collection_reference_folds_to_its_literal_topic() {
            let out = folded_arg(&format!(
                "{HOOK} {FORGE} export default function C() {{ \
                 const rows = useSharedSlot(guestbook); return <ul>{{rows}}</ul>; }}"
            ));
            assert_eq!(out, "\"guestbook\"");
        }

        /// The partition case: only the render knows the key, so the argument
        /// becomes a host lookup keyed by the BINDING name — which is exactly
        /// how the pure-Rust interpreter already resolves shared slots.
        #[test]
        fn a_partition_folds_to_a_host_lookup_keyed_by_binding_name() {
            let out = folded_arg(&format!(
                "{HOOK} {FORGE} export default function C({{ params }}) {{ \
                 const rows = useSharedSlot(messages.where({{ room: params.id }})); \
                 return <ul>{{rows}}</ul>; }}"
            ));
            assert_eq!(out, format!("{TOPIC_LOOKUP_FN}(\"rows\")"));
        }

        /// `.where` on something that is not a forge import is user code. The
        /// fold must not claim it — same import-pin discipline as the extractor.
        #[test]
        fn a_where_call_on_a_non_forge_value_is_left_alone() {
            let out = folded_arg(&format!(
                "{HOOK} import {{ messages }} from \"./mine\"; \
                 export default function C({{ params }}) {{ \
                 const rows = useSharedSlot(messages.where({{ room: params.id }})); \
                 return <ul>{{rows}}</ul>; }}"
            ));
            assert_eq!(out, "<member call>(…)");
        }

        /// No `albedo` import means no hook, means nothing to fold.
        #[test]
        fn a_module_without_the_hook_import_is_untouched() {
            let out = folded_arg(
                "import { messages } from \"albedo/forge\"; \
                 export default function C({ params }) { \
                 const rows = useSharedSlot(messages.where({ room: params.id })); \
                 return <ul>{rows}</ul>; }",
            );
            assert_eq!(out, "<member call>(…)");
        }
    }

    #[test]
    fn extracts_multiple_calls_in_source_order() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const a = useSharedSlot("topic-a");
                const b = useSharedSlot("topic-b");
                return <div>{a}{b}</div>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].static_topic(), Some("topic-a"));
        assert_eq!(bindings[0].hook_idx, 0);
        assert_eq!(bindings[1].static_topic(), Some("topic-b"));
        assert_eq!(bindings[1].hook_idx, 1);
    }

    #[test]
    fn ignores_calls_to_unrelated_use_shared_slot_imports() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "some-other-lib";
            export default function Component() {
                const x = useSharedSlot("nope");
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(bindings.is_empty());
    }

    #[test]
    fn ignores_calls_without_an_import_binding() {
        let bindings = extract_or_panic(
            r#"
            export default function Component() {
                const x = useSharedSlot("local");
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(bindings.is_empty());
    }

    #[test]
    fn rejects_call_inside_if_body() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                if (true) {
                    const x = useSharedSlot("conditional");
                }
                return <span/>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::HookInsideConditional { .. }));
    }

    #[test]
    fn rejects_call_inside_for_body() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                for (let i = 0; i < 3; i++) {
                    const x = useSharedSlot("loop");
                }
                return <span/>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::HookInsideConditional { .. }));
    }

    #[test]
    fn rejects_missing_topic_argument() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const x = useSharedSlot();
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::MissingTopicArgument { .. }));
    }

    #[test]
    fn rejects_non_string_literal_topic() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const t = "dynamic";
                const x = useSharedSlot(t);
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::NonStringLiteralTopic { .. }));
    }

    #[test]
    fn accepts_topic_wrapped_in_typescript_as_const_or_satisfies() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const a = useSharedSlot("a" as const);
                const b = useSharedSlot(("b"));
                return <div>{a}{b}</div>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].static_topic(), Some("a"));
        assert_eq!(bindings[1].static_topic(), Some("b"));
    }

    #[test]
    fn rejects_destructure_pattern() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const [x] = useSharedSlot("topic");
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::UnsupportedDestructurePattern));
    }

    #[test]
    fn extraction_is_deterministic_across_runs() {
        let source = r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const messages = useSharedSlot("chat:42");
                const cursors = useSharedSlot("cursors:doc-1");
                return <div>{messages}{cursors}</div>;
            }
        "#;
        let first = extract_or_panic(source);
        let second = extract_or_panic(source);
        assert_eq!(first, second);
    }
}
