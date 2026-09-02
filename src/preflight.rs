//! Every defect that is checkable before a request is served, in one place.
//!
//! # Why this module exists
//!
//! These checks were written at **boot**, because boot is where the compiled
//! project, the FORGE schema and the served route set first meet. That left
//! `albedo build` — the thing CI runs — passing an app that `albedo serve` would
//! refuse, so a broken build shipped and failed at container start instead of in
//! the pipeline that produced it.
//!
//! Running them from two places means writing them twice, and two spellings of a
//! rule disagree on exactly the inputs that matter. So they live here, take the
//! facts as arguments, and are called by both.
//!
//! # What they have in common
//!
//! Every one closes a **silent** failure — an app that built clean, booted
//! clean, and served HTTP 200 with something missing. Each was found by
//! mutating the scaffold the CLI generates and looking at what the tool said,
//! which was nothing. The shape repeats: a fact the compiler already computed,
//! with no consumer.
//!
//! | check | what was silent |
//! |---|---|
//! | [`literal_topics`] | a mistyped `useSharedSlot("…")` rendered an empty list forever |
//! | [`route_default_exports`] | a route with no `export default` served only the layout |
//! | [`form_actions`] | a `<form action="action:…">` naming nothing was a dead end at click time |
//! | [`npm_imports`] | an uninstalled package dropped the route's content |
//! | [`deferred_module_loads`] | a `require("…")` shipped verbatim and threw in the browser |
//! | [`partitioned_whole_reads`] | a whole read of a partitioned collection dropped the component |

use crate::bundler::npm::LoadForm;
use crate::forge::skeleton::ForgeSchema;
use crate::runtime::compiled::CompiledProject;

/// One problem, ready to print under a heading.
///
/// Carrying the heading with the lines is what lets a caller report several
/// unrelated failures in one pass instead of stopping at the first — the same
/// reason `forge::bindings` collects rather than short-circuits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// One line describing the whole class, e.g. *"a `useSharedSlot` topic is
    /// never written"*.
    pub heading: String,
    /// One line per offending site.
    pub problems: Vec<String>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:\n  - {}", self.heading, self.problems.join("\n  - "))
    }
}

/// A `useSharedSlot("literal")` naming a topic nothing can write.
#[must_use]
pub fn literal_topics(compiled: &CompiledProject, schema: &ForgeSchema) -> Option<Failure> {
    crate::forge::validate_literal_topic_reads(
        &compiled.literal_topic_reads(),
        schema,
        &compiled.writable_topics(),
    )
    .err()
    .map(|problems| Failure {
        heading: "a `useSharedSlot` topic is never written".to_string(),
        problems,
    })
}

/// A served route whose source module has no `export default`.
#[must_use]
pub fn route_default_exports(compiled: &CompiledProject, served: &[String]) -> Option<Failure> {
    let broken = compiled.routes_without_default_export(served);
    (!broken.is_empty()).then(|| Failure {
        heading: "these routes have no `export default`, so they would serve an empty page"
            .to_string(),
        problems: broken
            .iter()
            .map(|(route, spec)| format!("{route}  ({spec})"))
            .collect(),
    })
}

/// A `<form action="action:NAME">` naming an action nobody exported.
#[must_use]
pub fn form_actions(compiled: &CompiledProject) -> Option<Failure> {
    let unknown = compiled.forms_naming_unknown_actions();
    if unknown.is_empty() {
        return None;
    }
    let declared = compiled.declared_action_names();
    let declared = if declared.is_empty() {
        " (this project declares no actions)".to_string()
    } else {
        format!(" (declared: {})", declared.join(", "))
    };
    Some(Failure {
        heading: "a `<form action=\"action:…\">` names an action nobody exported".to_string(),
        problems: unknown
            .iter()
            .map(|(site, name)| format!("{site}: `{name}`{declared}"))
            .collect(),
    })
}

/// A bare import that resolves to no package.
#[must_use]
pub fn npm_imports(compiled: &CompiledProject) -> Option<Failure> {
    let unresolved = compiled.unresolved_npm_imports();
    (!unresolved.is_empty()).then(|| Failure {
        heading: "these imports resolve to no package, so any component using them renders nothing"
            .to_string(),
        problems: unresolved
            .iter()
            .map(|(spec, why)| format!("`{spec}` — {why}"))
            .collect(),
    })
}

/// A `require("…")` written in project source.
///
/// # Why this one is refused outright
///
/// `require` is a **parameter of the CJS factory wrapper**
/// (`function(module, exports, require, …)`) that `bundler::npm` emits around a
/// package's own files — never a global. Neither the QuickJS prelude nor
/// `assets/albedo-client.js` defines one, so a `require` an author writes in
/// project source is a `ReferenceError` on the server render path *and* in the
/// browser. There is no configuration in which it works, and no specifier that
/// rescues it — which is what separates it from a dynamic `import`.
///
/// 🔑 **The refusal is about the form, not the specifier.** `require("./util")`
/// is exactly as broken as `require("node:fs")`; naming built-ins here would
/// refuse the alarming half of a class that is entirely broken.
///
/// ⚖️ **Dynamic `import("…")` is deliberately NOT refused.**
/// `if (isNode) await import("fs")` is a standard isomorphic shape whose branch
/// never runs in a browser, and refusing it would break packages that work
/// today. It still ships unresolvable and still deserves a report; that report
/// needs a channel of its own rather than borrowing this one's severity, and is
/// tracked as the second half of `TODO.md` 9.7 Phase 3.
#[must_use]
pub fn deferred_module_loads(compiled: &CompiledProject) -> Option<Failure> {
    let requires: Vec<String> = compiled
        .deferred_module_loads()
        .iter()
        .filter(|(_, load)| load.form == LoadForm::Require)
        .map(|(module, load)| format!("{module}: `require(\"{}\")`", load.specifier))
        .collect();

    (!requires.is_empty()).then(|| Failure {
        heading: "`require(…)` is not available in project source — it is a CommonJS call, and \
                  neither the server renderer nor the browser defines one, so this throws a \
                  `ReferenceError` wherever the component runs. Use a static `import` instead"
            .to_string(),
        problems: requires,
    })
}

/// A `useSharedSlot(collection)` on a collection that declares `partition_by`.
///
/// Built clean, booted clean, and served **HTTP 200 with the reading
/// component's markup missing** — the sixth instance of this shape, and the
/// first found by building a real app rather than by mutating the scaffold.
/// The diagnosis and the reason it is a refusal rather than a feature live on
/// [`crate::forge::validate_partitioned_whole_reads`].
#[must_use]
pub fn partitioned_whole_reads(compiled: &CompiledProject, schema: &ForgeSchema) -> Option<Failure> {
    crate::forge::validate_partitioned_whole_reads(&compiled.literal_topic_reads(), schema)
        .err()
        .map(|problems| Failure {
            heading: "a partitioned collection has no whole-collection value, so this read renders nothing"
                .to_string(),
            problems,
        })
}

/// Run every check and collect what failed.
///
/// All of them, not the first: a `forge` block edited without its readers
/// usually breaks more than one thing, and fixing them one boot at a time is
/// the kind of small cruelty that makes a tool feel hostile.
#[must_use]
pub fn run(compiled: &CompiledProject, schema: &ForgeSchema, served: &[String]) -> Vec<Failure> {
    [
        npm_imports(compiled),
        deferred_module_loads(compiled),
        partitioned_whole_reads(compiled, schema),
        route_default_exports(compiled, served),
        form_actions(compiled),
        literal_topics(compiled, schema),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// [`run`], rendered as one message, or `Ok(())`.
///
/// # Errors
/// The joined report when anything failed.
pub fn check(
    compiled: &CompiledProject,
    schema: &ForgeSchema,
    served: &[String],
) -> Result<(), String> {
    let failures = run(compiled, schema, served);
    if failures.is_empty() {
        return Ok(());
    }
    Err(failures
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n\n"))
}

#[cfg(test)]
mod tests {
    /// 🔑 **A check that exists and is never called is this session's bug, in
    /// this file.**
    ///
    /// Every defect fixed today had that shape somewhere else —
    /// `FormExtract::action_name` extracted and never compared, `default_export`
    /// computed and never read, a resolver message written to a channel with no
    /// subscriber. [`super::run`] is the one list that makes a check reachable
    /// from both `albedo build` and boot, so a check added beside it but not
    /// *into* it would be the same mistake in the module written to prevent it.
    ///
    /// Derived from this file's own source, so it cannot drift the way a
    /// hand-maintained count would.
    #[test]
    fn every_check_in_this_module_is_called_by_run() {
        let source = include_str!("preflight.rs");

        let declared: Vec<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub fn "))
            .filter(|line| line.contains("-> Option<Failure>"))
            .filter_map(|line| line.split(['(', '<']).next())
            .collect();
        assert!(
            declared.len() >= 4,
            "expected the four checks this module was written with, found {declared:?}"
        );

        // The body of `run`, which is the only thing that makes a check reachable.
        let body = source
            .split_once("pub fn run(")
            .expect("run is defined here")
            .1;
        let body = body.split_once("\n}").expect("run has a body").0;

        for name in declared {
            assert!(
                body.contains(&format!("{name}(")),
                "`{name}` is a preflight check that `run` never calls, so neither \
                 `albedo build` nor boot will ever run it"
            );
        }
    }
}
