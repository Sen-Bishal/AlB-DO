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
    ImportSpecifier, Lit, MemberExpr, MemberProp, Module, ModuleDecl, ModuleExportName, ModuleItem,
    Pat, Prop, PropName, PropOrSpread, Stmt, Str, VarDeclarator,
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
/// `user.id` **is** a variant, as of item 5 P1. It is the only identity field
/// admitted, and that is forced rather than chosen — see [`Self::Identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// A route parameter: `params.id` on `/room/[id]`.
    Param(String),
    /// The authenticated principal: `user.id`.
    ///
    /// AUTH § 3 — *identity is one more key source, beside `params`*. The whole
    /// of derived authorization is this variant: `todos.where({ owner: user.id })`
    /// mints `todos:owner=u_7f3a`, and a session that is not `u_7f3a` cannot
    /// *name* that topic, so there is no policy to write and none to forget.
    ///
    /// **Only `id`.** `user.email` and friends are refused, and not out of
    /// caution: a key reaches a topic namespace, so it is bound by
    /// [`crate::runtime::broadcast::is_valid_partition_key`]'s
    /// `[A-Za-z0-9_-]{1,64}`. An email contains `@` and `.` and can never be a
    /// partition key; `PrincipalId` is minted as `u_` + uuid-simple precisely so
    /// that it always can. The alphabet is the reason the id is ours.
    ///
    /// **Carries no name**, unlike [`Self::Param`]. There is exactly one
    /// principal per request, so there is nothing to look up — which is also why
    /// the subscribe path can resolve this without a component render.
    Identity,
}

/// One argument to a declared APERTURE source route.
///
/// Unlike [`KeySource`], a literal is a first-class case here and not an
/// oversight. A FORGE partition key is dynamic by definition — a fixed one would
/// mean a collection with a single partition. An external resource is very often
/// fixed: `github.repo({ owner: "anthropics", name: "claude-code" })` is the
/// common spelling, not the exception.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceArg {
    /// A route parameter: `params.owner` on `/repo/[owner]`.
    Param(String),
    /// A string literal fixed at build time.
    Literal(String),
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
    /// APERTURE · one declared external resource:
    /// `useSharedSlot(github.repo({ owner: "anthropics", name: params.name }))`.
    ///
    /// Like [`TopicSpec::Partition`] it has no compile-time topic string when any
    /// argument is a param, so it is resolved per request by the same single
    /// resolver. Unlike it, the derivation behind the topic is an HTTP GET rather
    /// than a substrate query — which is precisely what `PRISM.md` § 13 required
    /// of any non-FORGE topic: *it must have a derivation*.
    Source {
        /// The declared source name (the `sources` block key), taken from the
        /// import's **export name** so an alias still resolves.
        source: String,
        /// The route name — the method called on the source.
        route: String,
        /// Arguments by parameter name, sorted so the recorded spec is identical
        /// on every build. The *minted identity* uses the route template's order,
        /// not this one; see `aperture::declare::source_topic_name`.
        args: Vec<(String, SourceArg)>,
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
            // A source is excluded even when every argument is a literal. Its
            // value is not in the broadcast registry at boot — it has to be
            // fetched — so pre-registering the name would publish an empty topic
            // as though it were an answer, which is the mistake `NoTopicWarmer`
            // exists to avoid.
            TopicSpec::Partition { .. } | TopicSpec::Source { .. } => None,
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
    /// An APERTURE source argument is `user.…`/`session.…`, which is real and
    /// scheduled — item 5 **P5** (`scope: "user"`), not P1. Distinguished from
    /// [`Self::PartitionKeyUnsupported`] so the author learns the feature is
    /// *coming*, not that they wrote nonsense.
    ///
    /// 🔑 No longer reachable from the *partition* path: `user.id` lands there
    /// as [`KeySource::Identity`] as of P1, and any other identity field is
    /// [`Self::UnsupportedIdentityField`] — a permanent refusal, not a schedule.
    IdentityKeyNotYetSupported { binding_name: Option<String> },
    /// The partition key is an identity field that can never be a partition key.
    ///
    /// Separate from [`Self::IdentityKeyNotYetSupported`] because the answer is
    /// different in kind: that one says *later*, this one says *never*. Only
    /// `user.id` is in the partition-key alphabet
    /// (`[A-Za-z0-9_-]{1,64}`) — `user.email` carries `@` and `.`, and a
    /// `session.*` field is a tab, not a human, so it is not an authorization
    /// basis at all.
    UnsupportedIdentityField {
        binding_name: Option<String>,
        found: String,
    },
    /// APERTURE · a source route call whose argument is not a single object
    /// literal of named arguments.
    UnsupportedSourceShape { binding_name: Option<String> },
    /// APERTURE · a source route argument the resolver cannot reproduce.
    SourceArgUnsupported {
        binding_name: Option<String>,
        arg: String,
        found: String,
    },
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
                 'albedo/forge', one partition of one (`messages.where({{ room: params.id }})`), \
                 or a declared source route imported from 'albedo/sources' \
                 (`github.repo({{ owner: \"anthropics\", name: \"claude-code\" }})`) \
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
                "useSharedSlot{}: a partition key must be a route parameter (`params.id`) or the \
                 signed-in principal (`user.id`); found `{found}`. The subscribe path resolves \
                 this binding from the route path and the session alone, with no component render \
                 to evaluate an expression in",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::UnsupportedIdentityField {
                binding_name,
                found,
            } => write!(
                f,
                "useSharedSlot{}: `{found}` cannot be a partition key — only `user.id` can. A key \
                 becomes a topic namespace, so it must match [A-Za-z0-9_-]{{1,64}}, and an email \
                 or a provider subject never will. (A `session.*` field is a browser tab, not a \
                 person, so it is not an authorization basis at all.) Store the field on the row \
                 and partition by `user.id`",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::IdentityKeyNotYetSupported { binding_name } => write!(
                f,
                "useSharedSlot{}: per-user *source* arguments (`user.id` on an APERTURE route) \
                 land with item 5 P5 (`scope: \"user\"`). Per-user FORGE partitions \
                 (`todos.where({{ owner: user.id }})`) and route parameters work today",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::UnsupportedSourceShape { binding_name } => write!(
                f,
                "useSharedSlot{}: a source route takes one object literal of named arguments, \
                 e.g. `github.repo({{ owner: \"anthropics\", name: params.name }})`, or no \
                 argument at all when the route's path has no placeholders",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::SourceArgUnsupported {
                binding_name,
                arg,
                found,
            } => write!(
                f,
                "useSharedSlot{}: source argument `{arg}` must be a string literal or a route \
                 parameter (`params.id`); found `{found}`. The subscribe path resolves this \
                 binding from the route path alone, with no component render to evaluate an \
                 expression in",
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
        // A call: either `messages.where({ room: params.id })` — one FORGE
        // partition — or `github.repo({ … })` — one declared source route. Both
        // are `<ident>.<method>(<object>)`, so the receiver's *import* decides
        // which, never the method name. A source route may legitimately be
        // called `where`.
        Expr::Call(inner) => extract_call_spec(inner, binding_name, imports),
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

/// The module specifier the compiler-generated source bindings come from.
/// Pinned exactly as [`FORGE_BINDINGS_MODULE`] is: a local variable that happens
/// to share a source's name is user code, not a source reference.
pub(crate) const SOURCE_BINDINGS_MODULE: &str = "albedo/sources";

/// The declared source name behind a local identifier, when it is bound by an
/// import from [`SOURCE_BINDINGS_MODULE`]. Returns the **export name**, so
/// `import { github as gh }` still resolves to `github`.
fn source_for_ident(local: &str, imports: &HashMap<String, ImportBinding>) -> Option<String> {
    imports
        .get(local)
        .filter(|binding| binding.source == SOURCE_BINDINGS_MODULE)
        .map(|binding| binding.export_name.clone())
}

/// Dispatch a `<ident>.<method>(…)` topic argument on what `<ident>` was
/// imported from.
///
/// Deciding on the *import* rather than the method name is what keeps the two
/// namespaces from constraining each other: a source is free to declare a route
/// named `where`, and a collection is free to be read by a source-shaped call
/// that simply is not one.
fn extract_call_spec(
    call: &CallExpr,
    binding_name: &Option<String>,
    imports: &HashMap<String, ImportBinding>,
) -> Result<TopicSpec, SharedSlotExtractError> {
    let unsupported = || SharedSlotExtractError::NonStringLiteralTopic {
        binding_name: binding_name.clone(),
    };
    let Callee::Expr(callee) = &call.callee else {
        return Err(unsupported());
    };
    let Expr::Member(member) = unwrap_parens(callee) else {
        return Err(unsupported());
    };
    let Expr::Ident(receiver) = unwrap_parens(&member.obj) else {
        return Err(unsupported());
    };

    if source_for_ident(receiver.sym.as_ref(), imports).is_some() {
        extract_source(call, member, binding_name, imports)
    } else {
        extract_partition(call, binding_name, imports)
    }
}

/// Lower `<source>.<route>({ <name>: <value>, … })`.
fn extract_source(
    call: &CallExpr,
    member: &MemberExpr,
    binding_name: &Option<String>,
    imports: &HashMap<String, ImportBinding>,
) -> Result<TopicSpec, SharedSlotExtractError> {
    let bad_shape = || SharedSlotExtractError::UnsupportedSourceShape {
        binding_name: binding_name.clone(),
    };

    let MemberProp::Ident(route) = &member.prop else {
        return Err(bad_shape());
    };
    let Expr::Ident(receiver) = unwrap_parens(&member.obj) else {
        return Err(bad_shape());
    };
    let source = source_for_ident(receiver.sym.as_ref(), imports).ok_or_else(bad_shape)?;

    // No argument is legal: a route whose path carries no `{placeholder}` needs
    // nothing bound. `github.status()`.
    let args = match call.args.as_slice() {
        [] => Vec::new(),
        [arg] => {
            let Expr::Object(object) = unwrap_parens(&arg.expr) else {
                return Err(bad_shape());
            };
            let mut collected: Vec<(String, SourceArg)> = Vec::with_capacity(object.props.len());
            for prop in &object.props {
                let PropOrSpread::Prop(prop) = prop else {
                    return Err(bad_shape());
                };
                let Prop::KeyValue(entry) = prop.as_ref() else {
                    return Err(bad_shape());
                };
                let name = match &entry.key {
                    PropName::Ident(ident) => ident.sym.to_string(),
                    PropName::Str(text) => text.value.to_string(),
                    _ => return Err(bad_shape()),
                };
                let value = extract_source_arg(&entry.value, &name, binding_name)?;
                collected.push((name, value));
            }
            // Sorted so the recorded spec is byte-identical on every build
            // regardless of how the author ordered the object literal. The
            // minted identity uses the route template's order instead, which is
            // why sorting here is free.
            collected.sort_by(|left, right| left.0.cmp(&right.0));
            collected
        }
        _ => return Err(bad_shape()),
    };

    Ok(TopicSpec::Source {
        source,
        route: route.sym.to_string(),
        args,
    })
}

/// Lower the value side of one `{ name: <here> }` source argument.
fn extract_source_arg(
    expr: &Expr,
    arg: &str,
    binding_name: &Option<String>,
) -> Result<SourceArg, SharedSlotExtractError> {
    match unwrap_parens(expr) {
        Expr::Lit(Lit::Str(text)) => Ok(SourceArg::Literal(text.value.to_string())),
        Expr::Member(member) => {
            if let (Expr::Ident(base), MemberProp::Ident(field)) =
                (unwrap_parens(&member.obj), &member.prop)
            {
                if base.sym.as_ref() == "params" {
                    return Ok(SourceArg::Param(field.sym.to_string()));
                }
                // Recognised on purpose, same as the partition path: the author
                // is asking for the right thing, it just is not wired yet.
                if matches!(base.sym.as_ref(), "user" | "session") {
                    return Err(SharedSlotExtractError::IdentityKeyNotYetSupported {
                        binding_name: binding_name.clone(),
                    });
                }
            }
            Err(SharedSlotExtractError::SourceArgUnsupported {
                binding_name: binding_name.clone(),
                arg: arg.to_string(),
                found: describe_expr(expr),
            })
        }
        other => Err(SharedSlotExtractError::SourceArgUnsupported {
            binding_name: binding_name.clone(),
            arg: arg.to_string(),
            found: describe_expr(other),
        }),
    }
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
                // AUTH item 5 P1. Only `id` — every other identity field fails
                // the partition-key alphabet, so admitting one would mint a
                // topic name the registry refuses at runtime instead of a build
                // error the author can read. See [`KeySource::Identity`].
                "user" if field.sym.as_ref() == "id" => return Ok(KeySource::Identity),
                "user" | "session" => {
                    return Err(SharedSlotExtractError::UnsupportedIdentityField {
                        binding_name: binding_name.clone(),
                        found: format!("{}.{}", base.sym, field.sym),
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
    let sources = import_locals(module, SOURCE_BINDINGS_MODULE);
    module.visit_mut_with(&mut TopicArgRewriter {
        hook_local,
        collections,
        sources,
    });
}

/// `local -> export_name` for every identifier imported from
/// [`FORGE_BINDINGS_MODULE`].
///
/// `pub(crate)` so the B2/B4 anchor markers
/// ([`crate::transforms::shared_slot_lists`]) resolve a `.where()` receiver to
/// its *collection* the same way this pass does. Two implementations of the
/// alias rule would let `import { messages as msgs }` classify under one name
/// and fan out under another.
pub(crate) fn forge_collection_locals(module: &Module) -> HashMap<String, String> {
    import_locals(module, FORGE_BINDINGS_MODULE)
}

/// `local -> export_name` for every identifier imported from
/// [`SOURCE_BINDINGS_MODULE`].
///
/// The APERTURE half of [`forge_collection_locals`], and `pub(crate)` for the
/// same reason: the B2/B4 anchor markers must resolve a source receiver exactly
/// as this pass does, or a binding gets extracted and subscribed without being
/// stamped — live on the wire, never painting.
pub(crate) fn source_locals(module: &Module) -> HashMap<String, String> {
    import_locals(module, SOURCE_BINDINGS_MODULE)
}

/// `local -> export_name` for every named import from `specifier`.
///
/// Extracted from [`forge_collection_locals`] when APERTURE needed the identical
/// rule for `albedo/sources`. Two copies of an alias rule is exactly how
/// `import { messages as msgs }` ends up classified under one name and resolved
/// under another.
fn import_locals(module: &Module, specifier: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for item in &module.body {
        let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item else {
            continue;
        };
        if import.src.value.as_ref() != specifier {
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
    sources: HashMap<String, String>,
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
            // A partition or a declared source route: only the render knows the
            // resolved topic, so both defer to the host through the *same*
            // lookup. APERTURE adds no new shim — a source topic reaches JS as
            // data exactly like a partition does, which is what keeps the naming
            // rule in one place (PRISM invariant 5).
            Expr::Call(inner)
                if self.is_where_on_collection(inner) || self.is_source_route_call(inner) =>
            {
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

    /// `<source>.<route>(…)` where `<source>` came from `albedo/sources`.
    ///
    /// The method name is deliberately unconstrained: a route may be called
    /// anything the author declared, including `where`. The receiver's import is
    /// the whole test, mirroring the extractor's dispatch.
    fn is_source_route_call(&self, call: &CallExpr) -> bool {
        let Callee::Expr(callee) = &call.callee else { return false };
        let Expr::Member(member) = unwrap_parens(callee) else { return false };
        if !matches!(&member.prop, MemberProp::Ident(_)) {
            return false;
        }
        matches!(unwrap_parens(&member.obj), Expr::Ident(receiver)
            if self.sources.contains_key(receiver.sym.as_ref()))
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
    /// AUTH item 5 P1 · the sentence the whole design is built on, as a test:
    /// *an authorization policy is never written — it is derived from the read
    /// that needs it.* This is the read. There is no policy anywhere in the
    /// source, and the binding still comes out keyed by the principal.
    fn a_user_id_partition_key_lowers_to_the_identity_key_source() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { todos } from "albedo/forge";
            export default function Component({ user }) {
                const rows = useSharedSlot(todos.where({ owner: user.id }));
                return <ul>{rows}</ul>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].binding_name, "rows");
        assert_eq!(
            bindings[0].spec,
            TopicSpec::Partition {
                collection: "todos".to_string(),
                column: "owner".to_string(),
                key: KeySource::Identity,
            },
            "`user.id` must lower to Identity — nothing else in P1 works if it does not"
        );
    }

    /// The refusal that has to survive P1, and the reason it is permanent rather
    /// than scheduled: a partition key becomes a topic namespace, so it lives in
    /// `[A-Za-z0-9_-]{1,64}`. An email never will.
    #[test]
    fn an_identity_field_that_is_not_id_is_refused_permanently() {
        for source_field in ["user.email", "user.name", "session.id"] {
            let (base, field) = source_field.split_once('.').expect("dotted");
            let err = extract_err(&format!(
                r#"
                import {{ useSharedSlot }} from "albedo";
                import {{ todos }} from "albedo/forge";
                export default function Component({{ {base} }}) {{
                    const rows = useSharedSlot(todos.where({{ owner: {base}.{field} }}));
                    return <ul>{{rows}}</ul>;
                }}
                "#,
            ));
            assert!(
                matches!(err, SharedSlotExtractError::UnsupportedIdentityField { .. }),
                "{source_field} got {err:?}"
            );
            let message = err.to_string();
            assert!(
                message.contains("only `user.id`"),
                "the message must say what *does* work: {message}"
            );
            // A permanent refusal must not read like a schedule — an author who
            // is told to wait for a feature that is never coming will wait.
            assert!(
                !message.contains("item 5"),
                "a permanent refusal must not name a milestone: {message}"
            );
        }
    }

    /// APERTURE's identity arguments are genuinely still scheduled (P5), so that
    /// path keeps the *later*-shaped message. Kept as its own test because the
    /// two refusals now say different things and confusing them would tell an
    /// author to wait for something that already shipped.
    #[test]
    fn an_aperture_source_argument_keyed_by_identity_still_names_its_milestone() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            import { gh } from "albedo/sources";
            export default function Component({ user }) {
                const repo = useSharedSlot(gh.repo({ owner: user.id }));
                return <div>{repo}</div>;
            }
            "#,
        );
        assert!(
            matches!(err, SharedSlotExtractError::IdentityKeyNotYetSupported { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("P5"),
            "the message must name the phase that unblocks it: {err}"
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

        const SOURCES: &str = "import { github } from \"albedo/sources\";";

        /// APERTURE · a declared source route folds to the **same** host lookup
        /// a partition does.
        ///
        /// The fold and the extractor must claim exactly the same calls. A
        /// source call the fold missed would survive into emitted JS as
        /// `github.repo({…})` — a call on an object QuickJS has never heard of,
        /// throwing at render time. One it folded that the extractor skipped
        /// would read a `host.topics` entry nobody filled, and be null forever.
        #[test]
        fn a_source_route_folds_to_a_host_lookup_keyed_by_binding_name() {
            let out = folded_arg(&format!(
                "{HOOK} {SOURCES} export default function C() {{ \
                 const repo = useSharedSlot(github.repo({{ owner: \"a\", name: \"b\" }})); \
                 return <div>{{repo}}</div>; }}"
            ));
            assert_eq!(out, format!("{TOPIC_LOOKUP_FN}(\"repo\")"));
        }

        /// A paramless route folds too — its identity still has to cross into JS
        /// as data rather than being rebuilt there.
        #[test]
        fn a_paramless_source_route_folds() {
            let out = folded_arg(&format!(
                "{HOOK} {SOURCES} export default function C() {{ \
                 const s = useSharedSlot(github.status()); return <div>{{s}}</div>; }}"
            ));
            assert_eq!(out, format!("{TOPIC_LOOKUP_FN}(\"s\")"));
        }

        /// The receiver's import decides, not the method name — so a source
        /// route named `where` folds as a source, and a collection's `.where`
        /// still folds as a partition. Both land on the same lookup, which is
        /// why this is about *claiming* the call rather than the output.
        #[test]
        fn a_source_route_named_where_is_still_folded() {
            let out = folded_arg(&format!(
                "{HOOK} {SOURCES} export default function C() {{ \
                 const hits = useSharedSlot(github.where({{ q: \"rust\" }})); \
                 return <div>{{hits}}</div>; }}"
            ));
            assert_eq!(out, format!("{TOPIC_LOOKUP_FN}(\"hits\")"));
        }

        /// A local object that merely shares a source's name is user code, and
        /// folding it would rewrite a call the extractor never claimed.
        #[test]
        fn a_call_on_a_non_source_value_is_left_alone() {
            let out = folded_arg(&format!(
                "{HOOK} import {{ github }} from \"./mine\"; \
                 export default function C() {{ \
                 const repo = useSharedSlot(github.repo({{ owner: \"a\" }})); \
                 return <div>{{repo}}</div>; }}"
            ));
            assert_eq!(out, "<member call>(…)");
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

    // ── APERTURE · declared source routes ────────────────────────────────

    #[test]
    fn a_source_route_with_literal_arguments_is_extracted() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { github } from "albedo/sources";
            export default function Component() {
                const repo = useSharedSlot(github.repo({ owner: "anthropics", name: "claude-code" }));
                return <div>{repo}</div>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].binding_name, "repo");
        assert_eq!(
            bindings[0].spec,
            TopicSpec::Source {
                source: "github".to_string(),
                route: "repo".to_string(),
                // Sorted by argument name, not source order.
                args: vec![
                    ("name".to_string(), SourceArg::Literal("claude-code".to_string())),
                    ("owner".to_string(), SourceArg::Literal("anthropics".to_string())),
                ],
            }
        );
        // Not a static topic: its value has to be fetched, so pre-registering
        // the name at boot would publish an empty topic as an answer.
        assert_eq!(bindings[0].static_topic(), None);
    }

    #[test]
    fn a_source_route_mixing_params_and_literals_is_extracted() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { github } from "albedo/sources";
            export default function Component({ params }) {
                const repo = useSharedSlot(github.repo({ owner: params.org, name: "claude-code" }));
                return <div>{repo}</div>;
            }
            "#,
        );
        assert_eq!(
            bindings[0].spec,
            TopicSpec::Source {
                source: "github".to_string(),
                route: "repo".to_string(),
                args: vec![
                    ("name".to_string(), SourceArg::Literal("claude-code".to_string())),
                    ("owner".to_string(), SourceArg::Param("org".to_string())),
                ],
            }
        );
    }

    #[test]
    fn a_paramless_source_route_needs_no_argument() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { status } from "albedo/sources";
            export default function Component() {
                const s = useSharedSlot(status.current());
                return <div>{s}</div>;
            }
            "#,
        );
        assert_eq!(
            bindings[0].spec,
            TopicSpec::Source {
                source: "status".to_string(),
                route: "current".to_string(),
                args: vec![],
            }
        );
    }

    /// An aliased import resolves to the declared name, exactly as a collection
    /// alias does. Two implementations of the alias rule is how a source gets
    /// classified under one name and resolved under another.
    #[test]
    fn an_aliased_source_import_resolves_to_its_export_name() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { github as gh } from "albedo/sources";
            export default function Component() {
                const repo = useSharedSlot(gh.repo({ owner: "a", name: "b" }));
                return <div>{repo}</div>;
            }
            "#,
        );
        let TopicSpec::Source { source, .. } = &bindings[0].spec else {
            panic!("expected a source spec");
        };
        assert_eq!(source, "github");
    }

    /// The receiver's *import* decides which derivation a call is, never the
    /// method name — so a source is free to declare a route called `where`.
    #[test]
    fn a_source_route_may_be_named_where() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { search } from "albedo/sources";
            export default function Component() {
                const hits = useSharedSlot(search.where({ q: "rust" }));
                return <div>{hits}</div>;
            }
            "#,
        );
        assert!(matches!(bindings[0].spec, TopicSpec::Source { .. }));
    }

    /// …and the mirror: a collection's `.where` is still a partition even
    /// though a source with the same local name would not be.
    #[test]
    fn a_collection_where_is_still_a_partition() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { messages } from "albedo/forge";
            export default function Component({ params }) {
                const rows = useSharedSlot(messages.where({ room: params.id }));
                return <div>{rows}</div>;
            }
            "#,
        );
        assert!(matches!(bindings[0].spec, TopicSpec::Partition { .. }));
    }

    /// A local object that merely looks like a source is user code. The import
    /// pin is the whole test.
    #[test]
    fn an_unimported_receiver_is_not_a_source() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            const github = { repo: () => "x" };
            export default function Component() {
                const repo = useSharedSlot(github.repo({ owner: "a" }));
                return <div>{repo}</div>;
            }
            "#,
        );
        assert!(matches!(
            err,
            SharedSlotExtractError::NonStringLiteralTopic { .. }
        ));
    }

    #[test]
    fn a_computed_source_argument_is_refused() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            import { github } from "albedo/sources";
            export default function Component({ props }) {
                const repo = useSharedSlot(github.repo({ owner: props.owner.toLowerCase() }));
                return <div>{repo}</div>;
            }
            "#,
        );
        assert!(matches!(
            err,
            SharedSlotExtractError::SourceArgUnsupported { .. }
        ));
    }

    #[test]
    fn a_user_scoped_source_argument_names_item_five() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            import { billing } from "albedo/sources";
            export default function Component() {
                const plan = useSharedSlot(billing.plan({ customer: user.id }));
                return <div>{plan}</div>;
            }
            "#,
        );
        assert!(matches!(
            err,
            SharedSlotExtractError::IdentityKeyNotYetSupported { .. }
        ));
        assert!(err.to_string().contains("item 5"));
    }

    #[test]
    fn a_non_object_source_argument_is_refused() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            import { github } from "albedo/sources";
            export default function Component() {
                const repo = useSharedSlot(github.repo("anthropics"));
                return <div>{repo}</div>;
            }
            "#,
        );
        assert!(matches!(
            err,
            SharedSlotExtractError::UnsupportedSourceShape { .. }
        ));
    }

    #[test]
    fn source_extraction_is_deterministic_regardless_of_argument_order() {
        // The recorded spec must not depend on how the author spelled the
        // object literal, or two builds of equivalent code would differ.
        let one = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { github } from "albedo/sources";
            export default function Component() {
                const repo = useSharedSlot(github.repo({ owner: "a", name: "b" }));
                return <div>{repo}</div>;
            }
            "#,
        );
        let two = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            import { github } from "albedo/sources";
            export default function Component() {
                const repo = useSharedSlot(github.repo({ name: "b", owner: "a" }));
                return <div>{repo}</div>;
            }
            "#,
        );
        assert_eq!(one[0].spec, two[0].spec);
    }
}
