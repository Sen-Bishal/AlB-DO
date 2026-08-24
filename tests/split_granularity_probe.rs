//! **The falsifier for `SCRIPTC_DIFF.md` § 3.** Measure, do not change.
//!
//! ## The claim under test
//!
//! scriptc fences at **statement** granularity: a statement it cannot compile becomes a
//! throw at that statement, and the coverage report lists it. `SCRIPTC_DIFF.md` § 3
//! proposed that the same move attacks our Tier-C share, because our fallback
//! granularity is the whole **component**.
//!
//! ## Half of that is already dead, by reading rather than measuring
//!
//! Every Tier-C branch in [`decide_tier_and_hydration`] is a **placement requirement**,
//! not a coverage refusal — *"a `useEffect` component must run its effect on mount"*,
//! *"must hydrate eagerly"*. A fence answers *"I cannot compile this"*; it has no answer
//! for *"this must execute in the browser."* Emitting a throw at a `useEffect` does not
//! make the app work, it breaks it. **The fence mechanism does not transfer.**
//!
//! ## What this file actually measures — the salvage
//!
//! The surviving question is not fencing but **splitting**: when a component ships
//! because of one hook, how much of it is static markup dragged along?
//!
//! For every Tier-C component, over the real corpus:
//!
//! * `total` — JSX elements in the component's own body.
//! * `shipped` — every element in a subtree rooted at a **reactive** node: one carrying
//!   an `on*` handler, or one whose own attributes/immediate expression children name a
//!   `useState` binding.
//! * `static_remainder` — `total - shipped`: what a perfect split would leave on the
//!   server.
//!
//! **Ancestors deliberately do not ship.** The metric assumes the best case for the
//! proposal — that a reactive subtree can be anchored in place (the `display:contents`
//! idea) while its ancestors stay server-rendered. If the number is bad even under the
//! most generous assumption, it is bad.
//!
//! ## Limits, stated so the number is not over-read
//!
//! * A `useState` binding is found only as `const [x, setX] = useState(...)`. Custom
//!   hooks returning state (`useQuery`, `useForm`) are **not** traced — those components
//!   will under-report `shipped`, biasing the result *toward* the proposal.
//! * Props are not traced. A prop that is reactive at the call site reads as static here.
//! * Element count is the unit, not bytes. A big static table and a small one weigh the
//!   same.
//! * `useEffect`-driven components ship for a reason this metric cannot see at all: the
//!   effect must run. Their static remainder is reported, but the effect body has to
//!   ship regardless of how much markup is static.
//!
//! ## 🔴 RESULT 2026-08-21 — the ranking selects for the probe's own blind spot
//!
//! Run over the 5-project corpus: 425 Tier-C components, **40.2% of Tier-C markup
//! measured as static**. That headline does not survive inspection.
//!
//! `DashboardLayout` (bulletproof-react) ranked #1 at 27 elements / 2 reactive /
//! **93% savable**. Read by hand, it calls `useNavigation`, `useAuthorization`,
//! `useLogout` and `useNavigate`; its nav list is built from
//! `checkAccess({ allowedRoles })`, and its `NavLink`s take
//! `className={({ isActive }) => …}`. Almost all of the "static" 25 is reactive
//! through channels this probe cannot see.
//!
//! That is not one bad row. **A component ranks high here precisely when its
//! reactivity arrives by an untraced channel**, so the top of the table is
//! anti-correlated with measurement accuracy and cannot be used as evidence.
//!
//! **What survives is the other tail, and it survives because the bias only runs one
//! way.** Under-tracing inflates `static`, so a component measured at ≤20% static is
//! *really* ≤20% static. **44.7% of Tier-C components are in that bucket** — for nearly
//! half the corpus a perfect split is worthless even under the most generous metric.
//!
//! 🔑 **Why this cannot be measured better today:** tracing `checkAccess` back to
//! `useAuthorization` is **cross-module hook resolution** — the same capability whose
//! absence makes `client_interactive` fire 534/536 (`TIER_DISTRIBUTION.md` Finding 1).
//! This question is blocked on the same `FactSource` seam (`TODO.md` P-b,
//! `EVOLUTION.md` § 2), and re-running this probe before that seam is cut will produce
//! the same unusable upper bound.
//!
//! Same discipline as `tiering_corpus_probe`: `#[ignore]`d, **asserts no threshold on
//! purpose.** A ratchet here would tempt us to sample components that flatter the idea.
//!
//! ```text
//! ALBEDO_TIERING_CORPUS=<manifest.json> \
//!   cargo test --test split_granularity_probe -- --ignored --nocapture
//! ```

use dom_render_compiler::effects::{decide_tier_and_hydration, TieringInputs, TieringReason};
use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::manifest::ManifestOptions;
use dom_render_compiler::scanner::{ProjectScanner, ScanMode};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use swc_common::{FileName, SourceMap};
use swc_ecma_ast::{
    Decl, ExportDefaultDecl, Expr, JSXAttrOrSpread, JSXElement, JSXElementChild,
    JSXExpr, Module, ModuleDecl, ModuleItem, Pat, Stmt,
};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitWith};

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

struct Corpus {
    name: String,
    dir: PathBuf,
}

fn corpora() -> Vec<Corpus> {
    let Ok(path) = std::env::var("ALBEDO_TIERING_CORPUS") else {
        return Vec::new();
    };
    let raw = std::fs::read_to_string(&path).expect("read corpus manifest");
    let entries: serde_json::Value = serde_json::from_str(&raw).expect("parse corpus manifest");
    entries
        .as_array()
        .expect("corpus manifest is an array")
        .iter()
        .filter(|entry| entry.get("kind").and_then(|k| k.as_str()) != Some("control"))
        .map(|entry| Corpus {
            name: entry["corpus"].as_str().expect("corpus name").to_string(),
            dir: PathBuf::from(entry["dir"].as_str().expect("corpus dir")),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// JSX analysis
// ---------------------------------------------------------------------------

fn syntax_for(ext: &str) -> Syntax {
    if matches!(ext, "ts" | "tsx") {
        Syntax::Typescript(TsSyntax {
            tsx: ext == "tsx",
            decorators: true,
            ..Default::default()
        })
    } else {
        Syntax::Es(EsSyntax {
            jsx: true,
            decorators: true,
            ..Default::default()
        })
    }
}

fn parse(source: &str, ext: &str) -> Option<Module> {
    let map = SourceMap::default();
    let file = map.new_source_file(FileName::Custom("probe".into()).into(), source.to_string());
    Parser::new(syntax_for(ext), StringInput::from(&*file), None)
        .parse_module()
        .ok()
}

/// `const [value, setValue] = useState(...)` — the first binding is state.
struct StateBindings {
    names: HashSet<String>,
}

impl Visit for StateBindings {
    fn visit_var_declarator(&mut self, decl: &swc_ecma_ast::VarDeclarator) {
        if let (Some(Pat::Array(array)), Some(init)) = (Some(&decl.name), decl.init.as_ref()) {
            if let Expr::Call(call) = &**init {
                let is_use_state = match &call.callee {
                    swc_ecma_ast::Callee::Expr(expr) => match &**expr {
                        Expr::Ident(id) => id.sym.as_ref() == "useState",
                        Expr::Member(member) => member
                            .prop
                            .as_ident()
                            .is_some_and(|p| p.sym.as_ref() == "useState"),
                        _ => false,
                    },
                    _ => false,
                };
                if is_use_state {
                    if let Some(Some(Pat::Ident(id))) = array.elems.first() {
                        self.names.insert(id.id.sym.to_string());
                    }
                }
            }
        }
        decl.visit_children_with(self);
    }
}

/// Any identifier in this expression that names a state binding.
struct NamesState<'a> {
    state: &'a HashSet<String>,
    found: bool,
}

impl Visit for NamesState<'_> {
    fn visit_ident(&mut self, id: &swc_ecma_ast::Ident) {
        if self.state.contains(id.sym.as_ref()) {
            self.found = true;
        }
    }
}

fn names_state(expr: &Expr, state: &HashSet<String>) -> bool {
    let mut probe = NamesState {
        state,
        found: false,
    };
    expr.visit_with(&mut probe);
    probe.found
}

/// Elements in this subtree, and how many of them a perfect split would ship.
struct Counts {
    total: usize,
    shipped: usize,
}

/// Is this element itself reactive — a handler, or an own expression naming state?
fn is_reactive(element: &JSXElement, state: &HashSet<String>) -> bool {
    for attr in &element.opening.attrs {
        let JSXAttrOrSpread::JSXAttr(attr) = attr else {
            // A spread could carry anything; treat it as reactive rather than
            // assume it is not. Rounding toward "ships" is the conservative
            // direction for a proposal that wants the number to be small.
            return true;
        };
        if let swc_ecma_ast::JSXAttrName::Ident(name) = &attr.name {
            if name.sym.starts_with("on") && name.sym.len() > 2 {
                return true;
            }
        }
        if let Some(swc_ecma_ast::JSXAttrValue::JSXExprContainer(container)) = &attr.value {
            if let JSXExpr::Expr(expr) = &container.expr {
                if names_state(expr, state) {
                    return true;
                }
            }
        }
    }
    // Immediate expression children only — a nested element's expressions belong
    // to that element, and are counted when it is visited.
    for child in &element.children {
        if let JSXElementChild::JSXExprContainer(container) = child {
            if let JSXExpr::Expr(expr) = &container.expr {
                if names_state(expr, state) {
                    return true;
                }
            }
        }
    }
    false
}

fn count_element(element: &JSXElement, state: &HashSet<String>, inside_reactive: bool) -> Counts {
    let reactive = inside_reactive || is_reactive(element, state);
    let mut counts = Counts {
        total: 1,
        shipped: usize::from(reactive),
    };
    for child in &element.children {
        let child_counts = match child {
            JSXElementChild::JSXElement(child) => count_element(child, state, reactive),
            JSXElementChild::JSXFragment(fragment) => count_fragment(fragment, state, reactive),
            _ => continue,
        };
        counts.total += child_counts.total;
        counts.shipped += child_counts.shipped;
    }
    counts
}

fn count_fragment(
    fragment: &swc_ecma_ast::JSXFragment,
    state: &HashSet<String>,
    inside_reactive: bool,
) -> Counts {
    let mut counts = Counts {
        total: 0,
        shipped: 0,
    };
    for child in &fragment.children {
        let child_counts = match child {
            JSXElementChild::JSXElement(child) => count_element(child, state, inside_reactive),
            JSXElementChild::JSXFragment(inner) => count_fragment(inner, state, inside_reactive),
            _ => continue,
        };
        counts.total += child_counts.total;
        counts.shipped += child_counts.shipped;
    }
    counts
}

/// Every top-level JSX tree in the named function, with its state bindings.
struct FunctionJsx {
    state: HashSet<String>,
    roots: Vec<JSXElement>,
    fragments: Vec<swc_ecma_ast::JSXFragment>,
}

struct CollectJsx {
    roots: Vec<JSXElement>,
    fragments: Vec<swc_ecma_ast::JSXFragment>,
}

impl Visit for CollectJsx {
    fn visit_jsx_element(&mut self, element: &JSXElement) {
        self.roots.push(element.clone());
        // Do not descend: nested elements are counted by `count_element`.
    }
    fn visit_jsx_fragment(&mut self, fragment: &swc_ecma_ast::JSXFragment) {
        self.fragments.push(fragment.clone());
    }
}

fn function_named(module: &Module, name: &str) -> Option<FunctionJsx> {
    fn body_of(module: &Module, name: &str) -> Option<swc_ecma_ast::BlockStmt> {
        for item in &module.body {
            let decl = match item {
                ModuleItem::Stmt(Stmt::Decl(decl)) => decl.clone(),
                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => export.decl.clone(),
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(ExportDefaultDecl {
                    decl: swc_ecma_ast::DefaultDecl::Fn(fn_expr),
                    ..
                })) => {
                    let matches = fn_expr
                        .ident
                        .as_ref()
                        .is_some_and(|id| id.sym.as_ref() == name);
                    if matches {
                        return fn_expr.function.body.clone();
                    }
                    continue;
                }
                _ => continue,
            };
            match decl {
                Decl::Fn(fn_decl) if fn_decl.ident.sym.as_ref() == name => {
                    return fn_decl.function.body.clone()
                }
                Decl::Var(var) => {
                    for declarator in &var.decls {
                        let Pat::Ident(id) = &declarator.name else {
                            continue;
                        };
                        if id.id.sym.as_ref() != name {
                            continue;
                        }
                        match declarator.init.as_deref() {
                            Some(Expr::Arrow(arrow)) => {
                                if let swc_ecma_ast::BlockStmtOrExpr::BlockStmt(block) = &*arrow.body
                                {
                                    return Some(block.clone());
                                }
                            }
                            Some(Expr::Fn(fn_expr)) => return fn_expr.function.body.clone(),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    let body = body_of(module, name)?;
    let mut state = StateBindings {
        names: HashSet::new(),
    };
    body.visit_with(&mut state);
    let mut jsx = CollectJsx {
        roots: Vec::new(),
        fragments: Vec::new(),
    };
    body.visit_with(&mut jsx);
    Some(FunctionJsx {
        state: state.names,
        roots: jsx.roots,
        fragments: jsx.fragments,
    })
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

struct Row {
    project: String,
    name: String,
    reason: TieringReason,
    total: usize,
    shipped: usize,
    state_bindings: usize,
}

impl Row {
    fn static_remainder(&self) -> usize {
        self.total.saturating_sub(self.shipped)
    }
    fn saved_pct(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.static_remainder() as f64 / self.total as f64) * 100.0
    }
}

fn measure(corpus: &Corpus) -> Vec<Row> {
    let scanner = ProjectScanner::new();
    let Ok(report) = scanner.scan_directory_with_mode(&corpus.dir, ScanMode::Lenient) else {
        eprintln!("SKIP {}: scan failed", corpus.name);
        return Vec::new();
    };
    let compiler = scanner.build_compiler(report.components);
    let options = ManifestOptions::default();
    let inputs = TieringInputs {
        tier_a_inline_max_bytes: options.tier_a_inline_max_bytes,
        tier_c_split_min_bytes: options.tier_c_split_min_bytes,
        tier_b_mode: options.tier_b_mode,
        tier_c_mode: options.tier_c_mode,
    };

    let mut rows = Vec::new();
    for component in compiler.graph().components() {
        if component.is_module_only {
            continue;
        }
        let lower = component.file_path.to_lowercase();
        if lower.contains(".test.")
            || lower.contains(".spec.")
            || lower.contains(".stories.")
            || lower.contains("/mocks/")
            || lower.contains("\\mocks\\")
        {
            continue;
        }
        let decision = decide_tier_and_hydration(
            component.effect_profile,
            component.is_interactive,
            component.is_client_interactive,
            component.state_escapes,
            component.reads_principal,
            component.imports_npm,
            component.is_above_fold,
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                component.weight as u64
            },
            inputs,
        );
        if decision.tier != Tier::C {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&component.file_path) else {
            continue;
        };
        let ext = Path::new(&component.file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("tsx");
        let Some(module) = parse(&source, ext) else {
            continue;
        };
        let Some(jsx) = function_named(&module, &component.name) else {
            continue;
        };
        let mut total = 0usize;
        let mut shipped = 0usize;
        for root in &jsx.roots {
            let counts = count_element(root, &jsx.state, false);
            total += counts.total;
            shipped += counts.shipped;
        }
        for fragment in &jsx.fragments {
            let counts = count_fragment(fragment, &jsx.state, false);
            total += counts.total;
            shipped += counts.shipped;
        }
        if total == 0 {
            continue;
        }
        rows.push(Row {
            project: corpus.name.clone(),
            name: component.name.clone(),
            reason: decision.reason,
            total,
            shipped,
            state_bindings: jsx.state.len(),
        });
    }
    rows
}

#[test]
#[ignore = "corpus probe; set ALBEDO_TIERING_CORPUS"]
fn how_much_of_a_tier_c_component_is_static() {
    let corpora = corpora();
    if corpora.is_empty() {
        eprintln!("SKIP: set ALBEDO_TIERING_CORPUS to a corpus JSON file");
        return;
    }

    let mut rows: Vec<Row> = corpora.iter().flat_map(|c| measure(c)).collect();
    if rows.is_empty() {
        eprintln!("SKIP: no Tier-C components analysable");
        return;
    }

    let mut out = String::new();
    let analysed = rows.len();
    let total_elements: usize = rows.iter().map(|r| r.total).sum();
    let total_shipped: usize = rows.iter().map(|r| r.shipped).sum();
    let total_static = total_elements - total_shipped;

    writeln!(out, "\n# Tier-C split granularity — {analysed} components analysed").unwrap();
    writeln!(
        out,
        "\ncorpus-wide: {total_elements} JSX elements in Tier-C components, \
         {total_shipped} reactive, {total_static} static ({:.1}% of Tier-C markup \
         a perfect split would leave on the server)",
        (total_static as f64 / total_elements as f64) * 100.0
    )
    .unwrap();

    // The ten worst — the components dragging the most static markup into a bundle.
    rows.sort_by_key(|r| std::cmp::Reverse(r.static_remainder()));
    writeln!(out, "\n## The ten worst (most static markup shipped)\n").unwrap();
    writeln!(
        out,
        "🔴 **This table is not evidence.** A component ranks high here exactly when its \
         reactivity arrives through a channel the probe cannot trace, so the ranking is \
         anti-correlated with measurement accuracy — verified by hand on the #1 row. \
         See the module header.\n"
    )
    .unwrap();
    writeln!(
        out,
        "| # | project | component | reason | elements | reactive | static | saved |"
    )
    .unwrap();
    writeln!(out, "|---|---|---|---|---:|---:|---:|---:|").unwrap();
    for (i, row) in rows.iter().take(10).enumerate() {
        writeln!(
            out,
            "| {} | {} | `{}` | {:?} | {} | {} | {} | **{:.0}%** |",
            i + 1,
            row.project,
            row.name,
            row.reason,
            row.total,
            row.shipped,
            row.static_remainder(),
            row.saved_pct()
        )
        .unwrap();
    }

    // Distribution, because ten components are an anecdote.
    let mut buckets = [0usize; 5];
    for row in &rows {
        let pct = row.saved_pct();
        let idx = if pct < 20.0 {
            0
        } else if pct < 40.0 {
            1
        } else if pct < 60.0 {
            2
        } else if pct < 80.0 {
            3
        } else {
            4
        };
        buckets[idx] += 1;
    }
    writeln!(out, "\n## Distribution — what fraction of each component is static\n").unwrap();
    writeln!(out, "| static share | components | share |").unwrap();
    writeln!(out, "|---|---:|---:|").unwrap();
    for (i, label) in ["0–20%", "20–40%", "40–60%", "60–80%", "80–100%"]
        .iter()
        .enumerate()
    {
        writeln!(
            out,
            "| {label} | {} | {:.1}% |",
            buckets[i],
            (buckets[i] as f64 / analysed as f64) * 100.0
        )
        .unwrap();
    }

    // By reason: a useEffect component ships its effect no matter how static its markup.
    writeln!(out, "\n## By reason\n").unwrap();
    writeln!(out, "| reason | components | elements | static | static share |").unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|").unwrap();
    let mut reasons: Vec<TieringReason> = rows.iter().map(|r| r.reason).collect();
    reasons.sort_by_key(|r| format!("{r:?}"));
    reasons.dedup_by_key(|r| format!("{r:?}"));
    for reason in reasons {
        let group: Vec<&Row> = rows.iter().filter(|r| r.reason == reason).collect();
        let elements: usize = group.iter().map(|r| r.total).sum();
        let statics: usize = group.iter().map(|r| r.static_remainder()).sum();
        writeln!(
            out,
            "| {reason:?} | {} | {elements} | {statics} | {:.1}% |",
            group.len(),
            (statics as f64 / elements.max(1) as f64) * 100.0
        )
        .unwrap();
    }

    let no_state = rows.iter().filter(|r| r.state_bindings == 0).count();
    writeln!(
        out,
        "\n⚠️ {no_state} of {analysed} ({:.1}%) have no `useState` binding this probe can \
         see — their reactive set is handler-only, so `shipped` is a LOWER bound and the \
         static share an UPPER bound. See the module header's limits.",
        (no_state as f64 / analysed as f64) * 100.0
    )
    .unwrap();

    println!("{out}");
    if let Ok(path) = std::env::var("ALBEDO_SPLIT_REPORT") {
        std::fs::write(&path, &out).expect("write report");
        eprintln!("report written to {path}");
    }
}
