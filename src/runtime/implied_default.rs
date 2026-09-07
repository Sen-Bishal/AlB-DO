//! Item 9.1a — a route component named with `export function Home` rather
//! than `export default function Home`.
//!
//! # Why this exists at all
//!
//! 📏 **62.3% of real React component files (694 of 1 114) use a named
//! export.** It is a *habit*, not a legacy-porting artifact: the scaffold
//! teaches `export default`, and a React developer's fingers type
//! `export function`. It was the first wall a stranger hit.
//!
//! # The rule, stated once
//!
//! A module with **no explicit default export** and **exactly one exported
//! component-shaped binding** treats that binding as its default. Zero or two
//! or more, and there is no implied default — the build keeps refusing, and
//! names what it found.
//!
//! 🔑 **Ambiguity must stay an error.** Guessing which of two exported
//! components is "the page" would be a silent wrong answer, which is worse
//! than the loud refusal this replaces.
//!
//! # Why it is a module rather than two `if` statements
//!
//! Two independent lowerings decide what a module's default is, and **both
//! run on the serve path**:
//!
//! * [`crate::runtime::eval::expr::parse_module`] → `ParsedModule::default_export`,
//!   which the Tier-A evaluator renders through and which
//!   `CompiledProject::routes_without_default_export` — the `preflight` refusal —
//!   reads.
//! * `quickjs_engine::lower_module_to_statements` → `__albedo_exports.default`,
//!   which is what the **live `albedo serve` path** actually invokes.
//!
//! 🪤 **If those two disagree, the failure is silent in the worst direction:**
//! preflight sees a default and passes the build, then QuickJS finds no
//! `.default` on the record and the route serves nothing. One definition, called
//! twice, is the only shape that cannot drift — the same correction item 13.4
//! made to the docker target, where a hand-maintained list shadowed a function
//! that already computed it.

use swc_ecma_ast::{Decl, Expr, FnDecl, ModuleDecl, ModuleItem, Pat, VarDecl};

/// Is this the name of something that could be a React component?
///
/// The capital is not a style preference — it is React's own dispatch rule.
/// JSX lowers a lowercase tag to a host element string and an uppercase one to
/// a component reference, so `export function helper()` is not a candidate for
/// "the page" no matter what it returns.
#[must_use]
pub fn is_component_name(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// The component-shaped names an `export function …` declaration introduces.
#[must_use]
pub fn component_names_in_fn_decl(fn_decl: &FnDecl) -> Option<String> {
    let name = fn_decl.ident.sym.to_string();
    is_component_name(&name).then_some(name)
}

/// The component-shaped names an `export const …` declaration introduces.
///
/// 🪤 **The initializer has to be a function.** `export const MAX_ITEMS = 10`
/// is capitalized and exported and is obviously not a page; counting it would
/// invent an ambiguity and refuse a module that has exactly one component.
/// `export const Home = () => …` is the arrow form of the same habit this item
/// exists for, so it must count.
#[must_use]
pub fn component_names_in_var_decl(var_decl: &VarDecl) -> Vec<String> {
    var_decl
        .decls
        .iter()
        .filter_map(|declarator| {
            let Pat::Ident(binding) = &declarator.name else {
                return None;
            };
            let name = binding.id.sym.to_string();
            if !is_component_name(&name) {
                return None;
            }
            let init = declarator.init.as_deref()?;
            matches!(init, Expr::Arrow(_) | Expr::Fn(_)).then_some(name)
        })
        .collect()
}

/// Every exported component-shaped binding in a module, in source order.
///
/// Source order matters only for the error message: a developer reading
/// "found: Header, Home" wants the order they wrote them in.
#[must_use]
pub fn exported_component_names(body: &[ModuleItem]) -> Vec<String> {
    let mut names = Vec::new();
    for item in body {
        let ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export_decl)) = item else {
            continue;
        };
        match &export_decl.decl {
            Decl::Fn(fn_decl) => names.extend(component_names_in_fn_decl(fn_decl)),
            Decl::Var(var_decl) => names.extend(component_names_in_var_decl(var_decl)),
            _ => {}
        }
    }
    names
}

/// The implied default for a module that declares none, or `None` when the
/// answer is not unambiguous.
#[must_use]
pub fn implied_default(candidates: &[String]) -> Option<String> {
    match candidates {
        [only] => Some(only.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swc_common::{FileName, SourceMap};
    use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};

    fn body_of(source: &str) -> Vec<ModuleItem> {
        let cm = SourceMap::default();
        let file = cm.new_source_file(FileName::Custom("t.tsx".into()).into(), source.to_string());
        let mut parser = Parser::new(
            Syntax::Typescript(TsSyntax {
                tsx: true,
                ..Default::default()
            }),
            StringInput::from(&*file),
            None,
        );
        parser.parse_module().expect("parses").body
    }

    #[test]
    fn a_single_named_function_export_is_the_implied_default() {
        let names = exported_component_names(&body_of("export function Home() { return null; }"));
        assert_eq!(names, vec!["Home".to_string()]);
        assert_eq!(implied_default(&names), Some("Home".to_string()));
    }

    /// The arrow form of the same habit — as common as the `function` form.
    #[test]
    fn a_single_named_arrow_export_is_the_implied_default() {
        let names = exported_component_names(&body_of("export const Home = () => null;"));
        assert_eq!(implied_default(&names), Some("Home".to_string()));
    }

    /// 🪤 The false-ambiguity trap: a capitalized constant is not a component,
    /// and counting it would refuse a module that has exactly one.
    #[test]
    fn a_capitalized_constant_is_not_a_component() {
        let names = exported_component_names(&body_of(
            "export const MAX_ITEMS = 10;\nexport function Home() { return null; }",
        ));
        assert_eq!(names, vec!["Home".to_string()]);
        assert_eq!(implied_default(&names), Some("Home".to_string()));
    }

    /// React's own dispatch rule: lowercase is a host element, never a page.
    #[test]
    fn a_lowercase_export_is_not_a_component() {
        let names = exported_component_names(&body_of("export function helper() { return 1; }"));
        assert!(names.is_empty());
        assert_eq!(implied_default(&names), None);
    }

    /// 🔑 Ambiguity stays an error — guessing would be a silent wrong answer.
    #[test]
    fn two_exported_components_have_no_implied_default() {
        let names = exported_component_names(&body_of(
            "export function Header() { return null; }\nexport function Home() { return null; }",
        ));
        assert_eq!(names, vec!["Header".to_string(), "Home".to_string()]);
        assert_eq!(implied_default(&names), None);
    }

    /// A module-local helper is not exported, so it cannot be the page — and
    /// must not create a false ambiguity with the one component that is.
    #[test]
    fn an_unexported_component_is_not_a_candidate() {
        let names = exported_component_names(&body_of(
            "function Sidebar() { return null; }\nexport function Home() { return null; }",
        ));
        assert_eq!(names, vec!["Home".to_string()]);
    }
}
