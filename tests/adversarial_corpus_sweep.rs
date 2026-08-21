//! The adversarial-value sweep: the whole corpus, re-rendered with markup-significant
//! characters in every string it interpolates.
//!
//! ## Why this exists
//!
//! The evaluator emitted **every** JSX expression child raw — `render_children` took an
//! `escape_expr_children: bool` that both call sites passed `false`, so the escaping
//! branch was unreachable. The two-renderer gate in `renderer_conformance.rs` *would*
//! have caught it: the renderers genuinely disagree, which is a `Diverge` by
//! construction. It never fired because **not one case in the corpus had a `<` in an
//! interpolated position.** Every fixture interpolated a number or a plain word.
//!
//! So the instrument was sound and the corpus was the limit. This file removes that
//! limit without authoring a single new component: it takes the corpus that already
//! exists, replaces the strings it interpolates with a hostile value, and runs the same
//! comparison over the result.
//!
//! ## The property, and why it needs no goldens
//!
//! The assertion is renderer-vs-renderer, exactly as the main gate is — so there is no
//! `expected.html` to author and none to drift. That is the point. A sweep whose
//! expected output had to be written by hand would cap the corpus at whatever anyone
//! had the patience to transcribe, and the escaping rules (`< > &` in text; those plus
//! `"` in attributes) are precisely the kind of thing a human transcribes wrong.
//!
//! ## What it does not claim
//!
//! Passing here is not proof that the escaping is *correct* — both renderers could be
//! wrong in the same direction. It proves they agree under hostile input, which is the
//! same contract the main gate carries. `tests/text_escaping_parity.rs` is where the
//! absolute rule is pinned against hand-written expected bytes; this file is where that
//! rule is checked across every construct the corpus contains.
//!
//! ## Reading a failure
//!
//! `DIVERGE` here means the two renderers treat a markup character differently in some
//! construct. The mutated source is printed with the failure — the fixture on disk is
//! untouched, so the printed source is the only copy of what actually failed.

use dom_render_compiler::conformance::{compare_entry, Contract};
use dom_render_compiler::runtime::eval::CompiledProject;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use swc_common::{FileName, SourceMap};
use swc_ecma_ast::{
    CallExpr, Callee, ExportAll, Expr, ImportDecl, Lit, Module, ModuleItem, NamedExport, Stmt, Str,
};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitWith};

/// Every character the two escape rules disagree about, in one value.
///
/// `<` `>` `&` must be escaped in a text position; `"` must be escaped in an attribute
/// and **not** in text; `'` in neither. The trailing `x` keeps the value from being
/// mistaken for punctuation in a diff.
const HOSTILE: &str = "<b>&\"'x";

/// Fixture groups swept. The same three the main gate walks — see the drift guard at
/// the bottom of this file, which fails if the two lists stop agreeing.
const FIXTURE_GROUPS: &[&str] = &["hook_compile", "jsx_matrix", "render_quickjs"];

/// Cases no hostile value can reach, each with the reason.
///
/// A case that carries nothing hostile is not a failure — `arithmetic` interpolates
/// numbers and has no strings at all — but it must be *named*, because a sweep that
/// silently substitutes nothing is a gate that passes without testing anything. That is
/// the exact failure class this file exists in response to, so it does not get to
/// recur inside the fix for it.
///
/// Two channels have to come up empty for a case to land here: no data string literal
/// in its source, and no prop it reads. Anything with either is exercised.
const NOT_EXERCISED: &[(&str, &str)] = &[
    ("jsx_matrix/arithmetic", "interpolates `a - b` over two number consts"),
    (
        "jsx_matrix/function_call_result",
        "interpolates `Math.floor(3.7)`",
    ),
    ("jsx_matrix/local_const", "interpolates the number 42"),
    (
        "jsx_matrix/new_date",
        "interpolates `new Date(0).toISOString()` — the argument is a number and \
         the output is generated, not authored",
    ),
    (
        "jsx_matrix/to_fixed",
        "interpolates `(ratio * 100).toFixed(1)` over a number const",
    ),
    (
        "hook_compile/counter",
        "seeds `useState(0)` and renders the count",
    ),
    (
        "hook_compile/counter_const",
        "same as counter, with the step hoisted to a const number",
    ),
    (
        "hook_compile/derived",
        "numeric state with a derived numeric expression",
    ),
    (
        "hook_compile/chained_consts",
        "a chain of numeric consts into one interpolation",
    ),
    (
        "hook_compile/usememo",
        "memoises a numeric computation over numeric state",
    ),
    (
        "hook_compile/conditional",
        "renders on a boolean, both branches static markup",
    ),
    (
        "hook_compile/conditional_dynamic",
        "same, with the condition derived from numeric state",
    ),
    (
        "hook_compile/event_reading_handler",
        "numeric state; the handler reads the event, not a string",
    ),
    (
        "hook_compile/text_template",
        "numeric state; its only literals are JSX attribute values \
         (`className`), which this sweep does not mutate — see `DataLiterals`",
    ),
    (
        "hook_compile/svg_icon",
        "boolean state; every literal is a JSX attribute value, which is where \
         the SVG casing contract lives — mutating those is `jsx_attributes.rs`'s \
         question, not this one",
    ),
    (
        "render_quickjs/form_errors",
        "every literal is a JSX attribute value, including the `action:NAME` \
         binding that makes it a form at all",
    ),
    (
        "render_quickjs/plain_post_form",
        "same: a plain POST form is entirely attribute literals",
    ),
    (
        "render_quickjs/form_in_list",
        "metadata-only fixture — `rows` resolves nowhere by design, so the main \
         gate does not render it either",
    ),
];

/// Known divergences under hostile input. Bidirectional, like the main gate's: an entry
/// here that starts passing fails the sweep, so a stale entry cannot outlive its bug.
const QUARANTINE: &[(&str, &str)] = &[(
    "render_quickjs/form_errors",
    "P6 form error-span PLACEMENT, quarantined identically in \
     `renderer_conformance.rs`. The evaluator interleaves each field's error span after \
     that field; the QuickJS shim is bottom-up and appends the set at the form's end. \
     Not an escaping difference and not made worse by hostile input — this entry exists \
     so the sweep inherits the main gate's known state rather than re-reporting it.",
)];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ---------------------------------------------------------------------------
// Finding the literals
// ---------------------------------------------------------------------------

/// Byte ranges of the string literals that carry author *data*.
///
/// Deliberately not every `Str` in the tree. Four kinds are skipped, and the reasons
/// differ enough to be worth stating separately:
///
/// * **Module specifiers** (`from "./config"`, `require("x")`, `import("x")`) — rewriting
///   one does not make a hostile value, it makes an unresolvable import.
/// * **Directive prologues** (`"use client"`) — semantic, not data.
/// * **JSX attribute literals** (`href="x"`) — JSX attribute strings are HTML-like: they
///   carry no backslash escapes, so a `"` inside one terminates it and the file stops
///   parsing. The escaping question they would answer is already answered by the
///   expression form (`href={v}`), which this sweep *does* mutate, so nothing is lost.
struct DataLiterals {
    spans: Vec<(usize, usize)>,
    base: u32,
}

impl DataLiterals {
    fn push(&mut self, s: &Str) {
        let lo = (s.span.lo.0 - self.base) as usize;
        let hi = (s.span.hi.0 - self.base) as usize;
        self.spans.push((lo, hi));
    }
}

impl Visit for DataLiterals {
    fn visit_str(&mut self, s: &Str) {
        self.push(s);
    }

    // Module specifiers: visit everything about the declaration except its source.
    fn visit_import_decl(&mut self, n: &ImportDecl) {
        n.specifiers.visit_with(self);
    }
    fn visit_named_export(&mut self, n: &NamedExport) {
        n.specifiers.visit_with(self);
    }
    fn visit_export_all(&mut self, _: &ExportAll) {}

    fn visit_call_expr(&mut self, n: &CallExpr) {
        let is_module_load = match &n.callee {
            Callee::Import(_) => true,
            Callee::Expr(expr) => {
                matches!(&**expr, Expr::Ident(id) if id.sym.as_ref() == "require")
            }
            Callee::Super(_) => false,
        };
        n.callee.visit_with(self);
        for (i, arg) in n.args.iter().enumerate() {
            if is_module_load && i == 0 {
                continue;
            }
            arg.visit_with(self);
        }
    }

    // JSX attribute values. `visit_jsx_attr` would still descend into an expression
    // container, which is the form we DO want, so only the bare-literal case is skipped.
    fn visit_jsx_attr(&mut self, n: &swc_ecma_ast::JSXAttr) {
        if let Some(swc_ecma_ast::JSXAttrValue::Lit(Lit::Str(_))) = &n.value {
            return;
        }
        n.value.visit_with(self);
    }
}

/// The prop names the default-exported component reads.
///
/// The second hostile channel, and the one that matters most: props are where data the
/// author did not write enters a render. `greeter` is the shape that makes the point —
/// it has no string literal anywhere, takes `{ initial, exclaim }`, and renders one of
/// them in a text position. Source mutation cannot reach it; only props can.
///
/// Both spellings are read, because the corpus uses both: a destructured parameter
/// (`function C({ a, b })`) and a named one dereferenced as `props.a`.
fn prop_names(module: &Module) -> Vec<String> {
    use swc_ecma_ast::{
        ArrowExpr, DefaultDecl, Decl, ExportDefaultDecl, ExportDefaultExpr, ObjectPatProp, Param,
        Pat,
    };

    fn from_pat(pat: &Pat, module: &Module) -> Vec<String> {
        match pat {
            Pat::Object(object) => object
                .props
                .iter()
                .filter_map(|prop| match prop {
                    ObjectPatProp::Assign(assign) => Some(assign.key.sym.to_string()),
                    ObjectPatProp::KeyValue(kv) => kv.key.as_ident().map(|i| i.sym.to_string()),
                    ObjectPatProp::Rest(_) => None,
                })
                .collect(),
            Pat::Ident(ident) => {
                // `props` — collect every `props.X` in the module.
                let mut found = MemberReads {
                    object: ident.id.sym.to_string(),
                    names: Vec::new(),
                };
                module.visit_with(&mut found);
                found.names
            }
            _ => Vec::new(),
        }
    }

    fn first_param(params: &[Param]) -> Option<&Pat> {
        params.first().map(|param| &param.pat)
    }

    for item in &module.body {
        let ModuleItem::ModuleDecl(decl) = item else {
            continue;
        };
        let pat = match decl {
            swc_ecma_ast::ModuleDecl::ExportDefaultDecl(ExportDefaultDecl {
                decl: DefaultDecl::Fn(fn_expr),
                ..
            }) => first_param(&fn_expr.function.params).cloned(),
            swc_ecma_ast::ModuleDecl::ExportDefaultExpr(ExportDefaultExpr { expr, .. }) => {
                match &**expr {
                    Expr::Arrow(ArrowExpr { params, .. }) => params.first().cloned(),
                    Expr::Fn(fn_expr) => first_param(&fn_expr.function.params).cloned(),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(pat) = pat {
            let mut names = from_pat(&pat, module);
            names.sort();
            names.dedup();
            return names;
        }
        // `export default function` may also arrive as a plain declaration.
        if let swc_ecma_ast::ModuleDecl::ExportDecl(export) = decl {
            if let Decl::Fn(fn_decl) = &export.decl {
                if let Some(pat) = first_param(&fn_decl.function.params) {
                    let mut names = from_pat(pat, module);
                    names.sort();
                    names.dedup();
                    return names;
                }
            }
        }
    }
    Vec::new()
}

/// Collects `<object>.<name>` reads for one object identifier.
struct MemberReads {
    object: String,
    names: Vec<String>,
}

impl Visit for MemberReads {
    fn visit_member_expr(&mut self, n: &swc_ecma_ast::MemberExpr) {
        if let (Expr::Ident(obj), Some(prop)) = (&*n.obj, n.prop.as_ident()) {
            if obj.sym.as_ref() == self.object {
                self.names.push(prop.sym.to_string());
            }
        }
        n.obj.visit_with(self);
    }
}

/// A directive prologue is an expression statement that is nothing but a string.
fn directive_spans(module: &Module, base: u32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for item in &module.body {
        let ModuleItem::Stmt(Stmt::Expr(expr_stmt)) = item else {
            break;
        };
        let Expr::Lit(Lit::Str(s)) = &*expr_stmt.expr else {
            break;
        };
        out.push(((s.span.lo.0 - base) as usize, (s.span.hi.0 - base) as usize));
    }
    out
}

fn syntax_for(ext: &str) -> Syntax {
    // The same choice `runtime/eval/expr.rs::parse_source` makes. If the sweep parsed
    // TSX differently from the compiler it would be measuring its own parser.
    if matches!(ext, "ts" | "tsx") {
        Syntax::Typescript(TsSyntax {
            tsx: ext == "tsx",
            decorators: true,
            ..Default::default()
        })
    } else {
        Syntax::Es(EsSyntax {
            jsx: matches!(ext, "jsx" | "js"),
            decorators: true,
            ..Default::default()
        })
    }
}

/// A JS string literal, quotes included, correctly escaped. JSON's string grammar is a
/// subset of JS's, so the serialiser already knows the rule and we do not re-spell it.
fn js_literal(value: &str) -> String {
    serde_json::to_string(value).expect("a string always serialises")
}

/// One mutated source file, plus what the sweep learned about it.
struct Mutated {
    source: String,
    /// How many data string literals were replaced. Load-bearing — see [`NOT_EXERCISED`].
    substitutions: usize,
    /// Prop names the default export reads, for the second hostile channel.
    props: Vec<String>,
}

/// Rewrite every data string literal in `source` to the hostile value.
fn make_hostile(source: &str, ext: &str) -> Result<Mutated, String> {
    let source_map = SourceMap::default();
    let file = source_map.new_source_file(
        FileName::Custom("sweep".to_string()).into(),
        source.to_string(),
    );
    let base = file.start_pos.0;
    let mut parser = Parser::new(syntax_for(ext), StringInput::from(&*file), None);
    let module = parser.parse_module().map_err(|err| format!("{err:?}"))?;

    let mut found = DataLiterals {
        spans: Vec::new(),
        base,
    };
    module.visit_with(&mut found);

    let skip = directive_spans(&module, base);
    let mut spans: Vec<(usize, usize)> = found
        .spans
        .into_iter()
        .filter(|span| !skip.contains(span))
        .collect();
    spans.sort_unstable();
    spans.dedup();

    let replacement = js_literal(HOSTILE);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (lo, hi) in &spans {
        if *lo < cursor || *hi > source.len() {
            return Err(format!("overlapping or out-of-range span {lo}..{hi}"));
        }
        out.push_str(&source[cursor..*lo]);
        out.push_str(&replacement);
        cursor = *hi;
    }
    out.push_str(&source[cursor..]);
    Ok(Mutated {
        source: out,
        substitutions: spans.len(),
        props: prop_names(&module),
    })
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

struct Case {
    /// `group/leaf`, matching the main gate's case naming so a failure here is
    /// greppable against a failure there.
    name: String,
    dir: PathBuf,
}

fn cases() -> Vec<Case> {
    let root = repo_root().join("tests").join("fixtures");
    let mut out = Vec::new();
    for group in FIXTURE_GROUPS {
        let Ok(read) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = read
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.join("Component.tsx").is_file())
            .collect();
        dirs.sort();
        for dir in dirs {
            let leaf = dir.file_name().unwrap().to_string_lossy().to_string();
            out.push(Case {
                name: format!("{group}/{leaf}"),
                dir,
            });
        }
    }
    out
}

/// Props whose type is not a string, and would break the component if made one.
///
/// Kept explicit and tiny. The alternative — inferring a prop's type — is a second,
/// worse copy of the type checker, and it would be wrong in exactly the cases that
/// matter. A name here is a claim that the prop is structural, not data.
const NON_STRING_PROPS: &[(&str, &str)] = &[(
    "render_quickjs/list",
    "items", // `.map()`ed; a string here tests the array path, not escaping
)];

/// Every prop the component reads, set to the hostile value.
///
/// Structural props keep a shape that works, with the hostile value carried *inside* —
/// so `list` still maps over an array, and what it maps over is hostile.
///
/// A prop the fixture declares as `number` (`stepper`'s `step`) receives the hostile
/// string too, which turns its `n + step` into concatenation. That is deliberate: the
/// sweep asks whether the two renderers agree when data arrives in a shape the author
/// did not expect, and a route param typed as a number in TypeScript is still a string
/// on the wire. The numeric path is what the main gate already covers.
fn hostile_props(case: &str, names: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    for name in names {
        let structural = NON_STRING_PROPS
            .iter()
            .any(|(c, prop)| *c == case && prop == name);
        let value = if structural {
            json!([HOSTILE, "beta"])
        } else {
            json!(HOSTILE)
        };
        map.insert(name.clone(), value);
    }
    Value::Object(map)
}

/// Copy a fixture into `dest`, rewriting every source file's data literals.
///
/// Returns the substitution count and the entry's prop names.
fn stage(case: &Case, dest: &Path) -> Result<(usize, Vec<String>), String> {
    let mut substitutions = 0usize;
    let mut props = Vec::new();
    let entries = std::fs::read_dir(&case.dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        let target = dest.join(&name);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        if matches!(ext.as_str(), "ts" | "tsx" | "js" | "jsx") {
            let source = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let mutated =
                make_hostile(&source, &ext).map_err(|e| format!("{}: {e}", path.display()))?;
            substitutions += mutated.substitutions;
            if name == "Component.tsx" {
                props = mutated.props.clone();
            }
            std::fs::write(&target, mutated.source).map_err(|e| e.to_string())?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| e.to_string())?;
        }
    }
    Ok((substitutions, props))
}

struct Outcome {
    name: String,
    substitutions: usize,
    props: usize,
    failures: Vec<String>,
}

impl Outcome {
    /// Whether anything hostile actually reached this case, by either channel.
    fn exercised(&self) -> bool {
        self.substitutions > 0 || self.props > 0
    }
}

fn sweep() -> Vec<Outcome> {
    let mut out = Vec::new();
    for case in cases() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (substitutions, prop_names) = match stage(&case, temp.path()) {
            Ok(staged) => staged,
            Err(err) => {
                out.push(Outcome {
                    name: case.name.clone(),
                    substitutions: 0,
                    props: 0,
                    failures: vec![format!("could not stage the mutated fixture: {err}")],
                });
                continue;
            }
        };

        let project = match CompiledProject::load_from_dir(temp.path()) {
            Ok(project) => project,
            Err(err) => {
                let source = std::fs::read_to_string(temp.path().join("Component.tsx"))
                    .unwrap_or_else(|_| "<unreadable>".to_string());
                out.push(Outcome {
                    name: case.name.clone(),
                    substitutions,
                    props: prop_names.len(),
                    failures: vec![format!(
                        "the mutated fixture does not compile: {err:#}\n--- mutated source ---\n{source}"
                    )],
                });
                continue;
            }
        };

        let props = hostile_props(&case.name, &prop_names);
        let mut failures = Vec::new();
        for contract in [Contract::Structural, Contract::Reactive] {
            let verdict = compare_entry(&project, "Component.tsx", &props, contract);
            let quarantined = QUARANTINE.iter().any(|(n, _)| *n == case.name);
            match (verdict.is_failure(), quarantined) {
                (true, false) => {
                    let source = std::fs::read_to_string(temp.path().join("Component.tsx"))
                        .unwrap_or_else(|_| "<unreadable>".to_string());
                    failures.push(format!(
                        "{contract:?}: {}\n{verdict:?}\n--- mutated source ---\n{source}",
                        verdict.label()
                    ));
                }
                (false, true) => failures.push(format!(
                    "{contract:?}: STALE QUARANTINE — this case passes now, delete its entry"
                )),
                _ => {}
            }
        }

        // `NotComparable` is a legitimate outcome of the main gate's setup limits, not
        // of hostile input, so it is not a failure here either.
        out.push(Outcome {
            name: case.name,
            substitutions,
            props: prop_names.len(),
            failures,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

/// The sweep. Every fixture, both contracts, hostile strings throughout.
#[test]
fn both_renderers_agree_under_hostile_values() {
    let outcomes = sweep();
    let literals: usize = outcomes.iter().map(|o| o.substitutions).sum();
    let props: usize = outcomes.iter().map(|o| o.props).sum();
    let exercised = outcomes.iter().filter(|o| o.exercised()).count();
    eprintln!(
        "\nadversarial sweep: {} cases, {exercised} carrying a hostile value \
         ({literals} literals, {props} props)",
        outcomes.len()
    );

    let problems: Vec<String> = outcomes
        .iter()
        .flat_map(|o| {
            o.failures
                .iter()
                .map(move |failure| format!("[{}] {failure}", o.name))
        })
        .collect();

    assert!(
        problems.is_empty(),
        "adversarial sweep: {} problem(s)\n\n{}\n",
        problems.len(),
        problems.join("\n\n")
    );
}

/// Every case must carry a hostile value, or be named in [`NOT_EXERCISED`] with a reason.
///
/// Without this the sweep degrades into a second, slower copy of the main gate the
/// moment the mutation stops finding literals — passing loudly while testing nothing.
/// That is the failure this whole file exists because of, so it gets its own gate
/// rather than a comment. Bidirectional, for the same reason the quarantine is: a case
/// listed here that starts carrying a hostile value has to lose its entry, or the list
/// becomes a place where coverage goes to be forgotten.
#[test]
fn every_case_carries_a_hostile_value_or_says_why_not() {
    let outcomes = sweep();

    let silent: Vec<&str> = outcomes
        .iter()
        .filter(|o| !o.exercised())
        .map(|o| o.name.as_str())
        .filter(|name| !NOT_EXERCISED.iter().any(|(n, _)| n == name))
        .collect();
    assert!(
        silent.is_empty(),
        "these cases carried nothing hostile and are not listed in NOT_EXERCISED — \
         either the mutation stopped reaching them, or they genuinely cannot be reached \
         and should be named with the reason:\n  {}",
        silent.join("\n  ")
    );

    let stale: Vec<&str> = NOT_EXERCISED
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| {
            outcomes
                .iter()
                .any(|o| o.name == *name && o.exercised())
        })
        .collect();
    assert!(
        stale.is_empty(),
        "these cases are listed in NOT_EXERCISED but now carry a hostile value — delete \
         their entries:\n  {}",
        stale.join("\n  ")
    );

    let exercised = outcomes.iter().filter(|o| o.exercised()).count();
    assert!(
        exercised >= 20,
        "only {exercised} cases carried a hostile value — the sweep has quietly stopped \
         covering the corpus"
    );
}

/// The sweep and the main gate must walk the same corpus.
///
/// Two copies of a discovery rule drift, and the one that drifts is always the one
/// nobody is looking at. If a fixture group is added to `renderer_conformance.rs` and
/// not here, the sweep silently stops covering it — so the group list is compared
/// against that file's, read from disk.
#[test]
fn the_sweep_walks_the_same_groups_as_the_main_gate() {
    let gate = std::fs::read_to_string(repo_root().join("tests").join("renderer_conformance.rs"))
        .expect("the main gate must exist");
    let line = gate
        .lines()
        .find(|line| line.contains("const FIXTURE_GROUPS"))
        .expect("renderer_conformance.rs must declare FIXTURE_GROUPS");
    for group in FIXTURE_GROUPS {
        assert!(
            line.contains(&format!("\"{group}\"")),
            "the sweep walks `{group}` and the main gate does not: {line}"
        );
    }
    let gate_groups = line.matches('"').count() / 2;
    assert_eq!(
        gate_groups,
        FIXTURE_GROUPS.len(),
        "the main gate walks {gate_groups} fixture groups and the sweep walks {}. \
         Add the missing group here, or the sweep covers less than the gate it \
         claims to extend: {line}",
        FIXTURE_GROUPS.len()
    );
}
