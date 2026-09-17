//! Finding the middleware, reading its `config`, and collecting what it loads.
//!
//! Everything here is read from the already-parsed project, so `albedo build`
//! and boot reach the same answer from the same facts — the rule
//! [`crate::preflight`] exists to keep.

use std::collections::HashSet;
use std::path::Path;

use swc_ecma_ast::{Expr, Lit, Prop, PropName, PropOrSpread};

use super::matcher::Matcher;
use crate::runtime::compiled::CompiledProject;

/// The file names a middleware may have, relative to the project root (`src/`).
pub const ENTRY_NAMES: &[&str] = &["middleware.ts", "middleware.tsx", "middleware.js", "middleware.jsx"];

/// A project's middleware, as the build found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiddlewareDecl {
    /// The entry's project-relative spec, e.g. `middleware.ts`.
    pub entry: String,
    /// Which requests enter it.
    pub matcher: Matcher,
}

/// Find and validate the project's middleware. `Ok(None)` when there is none.
///
/// # Errors
/// Every problem found, each naming the file.
pub fn declaration(project: &CompiledProject) -> Result<Option<MiddlewareDecl>, Vec<String>> {
    let mut problems = Vec::new();
    let root = project.project().root();

    // 🔴 Next.js reads `middleware.ts` from the project root as well as `src/`.
    // This project reads only `src/`, so a file pasted at the root would be
    // ignored — a guard that silently guards nothing. Refused by name instead.
    if let Some(project_dir) = root.parent() {
        for name in ENTRY_NAMES {
            if project_dir.join(name).is_file() && !same_dir(project_dir, root) {
                problems.push(format!(
                    "{name} is at the project root, where it is never loaded. Move it into \
                     {}/ — the directory `root` in albedo.config.ts points at",
                    root.file_name().and_then(|n| n.to_str()).unwrap_or("src")
                ));
            }
        }
    }

    let found: Vec<&str> = ENTRY_NAMES
        .iter()
        .copied()
        .filter(|name| project.module(name).is_some())
        .collect();

    let entry = match found.as_slice() {
        [] => {
            return if problems.is_empty() {
                Ok(None)
            } else {
                Err(problems)
            }
        }
        [one] => *one,
        many => {
            problems.push(format!(
                "more than one middleware file: {} — there can be only one, because they would \
                 otherwise run in an order nothing declares",
                many.join(", ")
            ));
            return Err(problems);
        }
    };

    let module = project.module(entry).expect("found above");
    if module.default_export.is_none() {
        problems.push(format!(
            "{entry} has no `export default` function, so there is nothing to run. Export the \
             middleware as the default: `export default function middleware(request) {{ … }}`"
        ));
    }

    let matcher = match module
        .module_constants
        .iter()
        .find(|(name, _)| name == "config")
    {
        None => Matcher::all(),
        Some((_, expr)) => match matcher_from_config(expr) {
            Ok(Some(sources)) => match Matcher::compile(&sources) {
                Ok(matcher) => matcher,
                Err(errors) => {
                    problems.extend(errors.into_iter().map(|e| format!("{entry}: {e}")));
                    Matcher::all()
                }
            },
            Ok(None) => Matcher::all(),
            Err(message) => {
                problems.push(format!("{entry}: {message}"));
                Matcher::all()
            }
        },
    };

    if let Err(message) = module_graph(project, entry) {
        problems.push(message);
    }

    if problems.is_empty() {
        Ok(Some(MiddlewareDecl {
            entry: entry.to_string(),
            matcher,
        }))
    } else {
        Err(problems)
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Lower `export const config = { matcher }`. `Ok(None)` when `matcher` is
/// absent.
///
/// 🔴 **Literal-only, and anything else is an error** — the rule
/// `auth_from_const_expr` set. A matcher that cannot be read at build time would
/// have to fall back to *something*, and both fallbacks are wrong: "every
/// request" runs code on paths the author excluded, "no request" silently
/// disables a guard.
fn matcher_from_config(expr: &Expr) -> Result<Option<Vec<String>>, String> {
    let Expr::Object(object) = unparen(expr) else {
        return Err("`export const config` must be an object literal".to_string());
    };

    let mut matcher = None;
    for prop in &object.props {
        let PropOrSpread::Prop(prop) = prop else {
            return Err("`config` cannot use a spread — every key must be written out".to_string());
        };
        let Prop::KeyValue(kv) = &**prop else {
            return Err("`config` must use `key: value` properties".to_string());
        };
        let key = match &kv.key {
            PropName::Ident(ident) => ident.sym.to_string(),
            PropName::Str(s) => s.value.to_string(),
            _ => return Err("`config` keys must be plain names".to_string()),
        };
        match key.as_str() {
            "matcher" => matcher = Some(matcher_sources(&kv.value)?),
            // A misspelled `matchers` would otherwise leave the middleware
            // running on every request while its author believes it is scoped.
            other => {
                return Err(format!(
                    "`config.{other}` is not supported; the only key is `matcher`"
                ))
            }
        }
    }
    Ok(matcher)
}

fn matcher_sources(expr: &Expr) -> Result<Vec<String>, String> {
    let not_literal = || {
        "`config.matcher` must be a string literal or an array of string literals — it is read \
         at build time, and a matcher that cannot be read there has no safe default"
            .to_string()
    };
    match unparen(expr) {
        Expr::Array(array) => array
            .elems
            .iter()
            .map(|elem| match elem {
                Some(elem) if matches!(unparen(&elem.expr), Expr::Object(_)) => Err(
                    "`config.matcher` objects (`{ source, has, missing }`) are not supported; \
                     use path strings"
                        .to_string(),
                ),
                Some(elem) if elem.spread.is_none() => {
                    string_literal(&elem.expr).ok_or_else(not_literal)
                }
                _ => Err(not_literal()),
            })
            .collect(),
        other => string_literal(other).map(|s| vec![s]).ok_or_else(not_literal),
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match unparen(expr) {
        Expr::Lit(Lit::Str(s)) => Some(s.value.to_string()),
        Expr::Tpl(tpl) if tpl.exprs.is_empty() && tpl.quasis.len() == 1 => {
            let quasi = &tpl.quasis[0];
            Some(
                quasi
                    .cooked
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| quasi.raw.to_string()),
            )
        }
        _ => None,
    }
}

fn unparen(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(paren) => unparen(&paren.expr),
        Expr::TsAs(as_expr) => unparen(&as_expr.expr),
        Expr::TsConstAssertion(assertion) => unparen(&assertion.expr),
        Expr::TsSatisfies(satisfies) => unparen(&satisfies.expr),
        other => other,
    }
}

/// The middleware entry and every project module it reaches, **dependencies
/// first**, each with its source — the load order an engine needs.
///
/// npm packages are not in the list: they are installed on every pool engine at
/// boot from the project's bundles, which already include what this file imports.
///
/// # Errors
/// A relative import that names no project module, naming the importer and the
/// specifier.
pub fn module_graph(
    project: &CompiledProject,
    entry: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut order = Vec::new();
    let mut visited = HashSet::new();
    visit(project, entry, &mut visited, &mut order)?;
    Ok(order)
}

fn visit(
    project: &CompiledProject,
    spec: &str,
    visited: &mut HashSet<String>,
    order: &mut Vec<(String, String)>,
) -> Result<(), String> {
    if !visited.insert(spec.to_string()) {
        return Ok(());
    }
    let components = project.project();
    let source = components
        .module_source(spec)
        .ok_or_else(|| format!("{spec}: source not found"))?;
    for specifier in crate::bundler::npm::scan_relative_imports(source) {
        let resolved = components
            .resolve_project_import(spec, &specifier)
            .ok_or_else(|| {
                format!("{spec}: import \"{specifier}\" does not resolve to a file in the project")
            })?;
        visit(project, &resolved, visited, order)?;
    }
    order.push((spec.to_string(), source.to_string()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real project on disk, loaded the way boot and build load one — so
    /// these exercise the parser's actual view of the file, not a hand-built
    /// `ParsedModule` that could agree with a broken reader.
    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, CompiledProject) {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("routes")).unwrap();
        std::fs::write(
            src.join("routes/index.tsx"),
            "export default function Home() { return <main>home</main>; }",
        )
        .unwrap();
        for (path, body) in files {
            let full = if let Some(rest) = path.strip_prefix("../") {
                dir.path().join(rest)
            } else {
                src.join(path)
            };
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        let compiled = CompiledProject::load_from_dir(&src).expect("project loads");
        (dir, compiled)
    }

    #[test]
    fn no_middleware_file_is_no_middleware() {
        let (_dir, compiled) = project(&[]);
        assert_eq!(declaration(&compiled), Ok(None));
    }

    #[test]
    fn a_middleware_without_config_runs_on_everything() {
        let (_dir, compiled) = project(&[(
            "middleware.ts",
            "export default function middleware(request) { return; }",
        )]);
        let decl = declaration(&compiled).expect("valid").expect("present");
        assert_eq!(decl.entry, "middleware.ts");
        assert!(decl.matcher.matches("/logo.png"));
    }

    #[test]
    fn the_matcher_is_read_from_config_in_both_spellings() {
        let (_dir, compiled) = project(&[(
            "middleware.ts",
            r#"
                export const config = { matcher: ["/admin/:path*", `/account`] } as const;
                export default function middleware() {}
            "#,
        )]);
        let decl = declaration(&compiled).unwrap().unwrap();
        assert!(decl.matcher.matches("/admin/x"));
        assert!(decl.matcher.matches("/account"));
        assert!(!decl.matcher.matches("/"));

        let (_dir, compiled) = project(&[(
            "middleware.ts",
            r#"export const config = { matcher: "/only" }; export default function m() {}"#,
        )]);
        let decl = declaration(&compiled).unwrap().unwrap();
        assert_eq!(decl.matcher.sources(), Some(vec!["/only"]));
    }

    #[test]
    fn an_unreadable_or_misspelled_config_is_refused_not_defaulted() {
        for (config, needle) in [
            ("const paths = ['/a']; export const config = { matcher: paths };", "string literal"),
            ("export const config = { matchers: ['/a'] };", "`config.matchers`"),
            ("export const config = { matcher: ['/a/(.*)'] };", "regex"),
            ("export const config = { matcher: [{ source: '/a' }] };", "objects"),
            ("export const config = { matcher: [] };", "could never run"),
        ] {
            let source = format!("{config}\nexport default function middleware() {{}}");
            let (_dir, compiled) = project(&[("middleware.ts", source.as_str())]);
            let problems = declaration(&compiled).expect_err(config);
            assert!(
                problems.iter().any(|p| p.contains(needle) && p.contains("middleware.ts")),
                "{config}: {problems:?}"
            );
        }
    }

    #[test]
    fn a_middleware_with_nothing_to_run_is_refused() {
        let (_dir, compiled) = project(&[("middleware.ts", "export const config = {};")]);
        let problems = declaration(&compiled).expect_err("no default");
        assert!(problems[0].contains("no `export default`"), "{problems:?}");
    }

    #[test]
    fn two_middleware_files_are_refused() {
        let (_dir, compiled) = project(&[
            ("middleware.ts", "export default function a() {}"),
            ("middleware.js", "export default function b() {}"),
        ]);
        let problems = declaration(&compiled).expect_err("two");
        assert!(problems[0].contains("more than one"), "{problems:?}");
    }

    /// 🔴 The Next.js location. Silently ignoring it would be a guard that
    /// guards nothing.
    #[test]
    fn a_middleware_at_the_project_root_is_refused_by_name() {
        let (_dir, compiled) = project(&[("../middleware.ts", "export default function m() {}")]);
        let problems = declaration(&compiled).expect_err("root-level");
        assert!(problems[0].contains("project root"), "{problems:?}");
    }

    #[test]
    fn the_module_graph_is_dependencies_first_and_names_a_missing_import() {
        let (_dir, compiled) = project(&[
            ("lib/a.ts", "import './b'; export const a = 1;"),
            ("lib/b.ts", "export const b = 2;"),
            (
                "middleware.ts",
                "import { a } from './lib/a'; import { redirect } from 'albedo/middleware';\nexport default function m() { return a; }",
            ),
        ]);
        let order: Vec<String> = module_graph(&compiled, "middleware.ts")
            .expect("resolves")
            .into_iter()
            .map(|(spec, _)| spec)
            .collect();
        assert_eq!(order, vec!["lib/b.ts", "lib/a.ts", "middleware.ts"]);

        let (_dir, compiled) = project(&[(
            "middleware.ts",
            "import { x } from './nope';\nexport default function m() { return x; }",
        )]);
        let problems = declaration(&compiled).expect_err("missing import");
        assert!(problems.iter().any(|p| p.contains("./nope")), "{problems:?}");
    }
}
