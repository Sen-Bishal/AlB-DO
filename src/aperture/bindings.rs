//! APERTURE · A1 — the boot check where components and the `sources` block meet.
//!
//! The sibling of `forge/bindings.rs`, and it exists for the identical reason.
//! Everything up to this point validates one side in isolation: the extractor
//! records what a component *wrote* with no config to check it against, and
//! `SourceRegistry` validates the config with no idea which components read it.
//!
//! So a call naming a route that does not exist, or omitting an argument the
//! route's path requires, is well-formed on both sides and wrong in the join.
//! Its failure mode is the worst kind — the binding resolves to nothing, the
//! slot renders empty forever, and there is no error anywhere. That is what this
//! module turns into a build error.

use crate::aperture::declare::SourceRegistry;
use crate::aperture::PathSegment;
use std::collections::BTreeSet;

/// One `useSharedSlot(<source>.<route>({ … }))` a component performs.
///
/// Ordered so a project with several problems reports them identically on every
/// machine.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceBinding {
    /// Module specifier the component lives in.
    pub module: String,
    /// Component function name.
    pub component: String,
    /// The local name the binding is assigned to.
    pub binding: String,
    /// The source it names.
    pub source: String,
    /// The route it calls.
    pub route: String,
    /// Argument names supplied at the call site, sorted.
    pub args: Vec<String>,
}

impl SourceBinding {
    fn site(&self) -> String {
        format!("{}::{} `{}`", self.module, self.component, self.binding)
    }
}

/// Why a component's source read does not match the declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceBindingProblem {
    /// The `sources` block has no such source.
    UnknownSource {
        /// Where the call is.
        site: String,
        /// The name it used.
        source: String,
        /// What is declared, for the "did you mean" line.
        declared: Vec<String>,
    },
    /// The source exists but declares no such route.
    UnknownRoute {
        /// Where the call is.
        site: String,
        /// Source name.
        source: String,
        /// Route name it used.
        route: String,
        /// Routes that source does declare.
        declared: Vec<String>,
    },
    /// The route's path has a placeholder the call site never supplies.
    MissingArgument {
        /// Where the call is.
        site: String,
        /// The qualified route.
        route: String,
        /// The placeholder with nothing bound to it.
        argument: String,
    },
    /// The call site supplies an argument the route's path has no hole for.
    ///
    /// Refused rather than ignored: an extra argument is almost always a typo of
    /// a real one, and silently dropping it produces a URL that is missing a
    /// value the author believes they passed.
    UnknownArgument {
        /// Where the call is.
        site: String,
        /// The qualified route.
        route: String,
        /// The unexpected argument.
        argument: String,
        /// What the path actually takes.
        expected: Vec<String>,
    },
}

impl std::fmt::Display for SourceBindingProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSource {
                site,
                source,
                declared,
            } => write!(
                f,
                "{site} reads source `{source}`, which is not declared in the `sources` block \
                 (declared: {})",
                list(declared)
            ),
            Self::UnknownRoute {
                site,
                source,
                route,
                declared,
            } => write!(
                f,
                "{site} calls `{source}.{route}(…)`, but `{source}` declares no route `{route}` \
                 (declared: {})",
                list(declared)
            ),
            Self::MissingArgument {
                site,
                route,
                argument,
            } => write!(
                f,
                "{site} calls `{route}(…)` without `{argument}`, which its `path` requires. \
                 The topic would resolve to nothing and the slot would stay empty"
            ),
            Self::UnknownArgument {
                site,
                route,
                argument,
                expected,
            } => write!(
                f,
                "{site} passes `{argument}` to `{route}(…)`, whose `path` has no such \
                 placeholder (takes: {})",
                list(expected)
            ),
        }
    }
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items
            .iter()
            .map(|item| format!("`{item}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Check every component's source reads against the declared `sources` block.
///
/// # Errors
/// Every problem found, in a stable order, so one boot reports all of them
/// rather than making the author fix them one build at a time.
pub fn validate_source_bindings(
    bindings: &[SourceBinding],
    registry: &SourceRegistry,
) -> Result<(), Vec<SourceBindingProblem>> {
    let declared_sources: BTreeSet<&str> =
        registry.iter().map(|route| route.source.as_str()).collect();

    let mut problems = Vec::new();
    for binding in bindings {
        let site = binding.site();

        let Some(route) = registry.get(&binding.source, &binding.route) else {
            if declared_sources.contains(binding.source.as_str()) {
                problems.push(SourceBindingProblem::UnknownRoute {
                    site,
                    source: binding.source.clone(),
                    route: binding.route.clone(),
                    declared: registry
                        .iter()
                        .filter(|candidate| candidate.source == binding.source)
                        .map(|candidate| candidate.route.clone())
                        .collect(),
                });
            } else {
                problems.push(SourceBindingProblem::UnknownSource {
                    site,
                    source: binding.source.clone(),
                    declared: declared_sources.iter().map(|s| (*s).to_string()).collect(),
                });
            }
            continue;
        };

        let expected: Vec<String> = route
            .segments
            .iter()
            .filter_map(|segment| match segment {
                PathSegment::Param(name) => Some(name.clone()),
                PathSegment::Literal(_) => None,
            })
            .collect();

        for placeholder in &expected {
            if !binding.args.iter().any(|arg| arg == placeholder) {
                problems.push(SourceBindingProblem::MissingArgument {
                    site: site.clone(),
                    route: route.qualified_name(),
                    argument: placeholder.clone(),
                });
            }
        }
        for supplied in &binding.args {
            if !expected.iter().any(|placeholder| placeholder == supplied) {
                problems.push(SourceBindingProblem::UnknownArgument {
                    site: site.clone(),
                    route: route.qualified_name(),
                    argument: supplied.clone(),
                    expected: expected.clone(),
                });
            }
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aperture::declare::{RouteDecl, SourceDecl};
    use std::collections::BTreeMap;

    fn registry() -> SourceRegistry {
        let mut routes = BTreeMap::new();
        routes.insert(
            "repo".to_string(),
            RouteDecl {
                path: "/repos/{owner}/{name}".to_string(),
                refresh: None,
                method: None,
            },
        );
        routes.insert(
            "status".to_string(),
            RouteDecl {
                path: "/status".to_string(),
                refresh: None,
                method: None,
            },
        );
        let decls: BTreeMap<String, SourceDecl> = [(
            "github".to_string(),
            SourceDecl {
                base: "https://api.github.com".to_string(),
                auth: None,
                headers: BTreeMap::new(),
                routes,
            },
        )]
        .into_iter()
        .collect();
        SourceRegistry::from_declarations(&decls, |_| None).expect("lowers")
    }

    fn binding(source: &str, route: &str, args: &[&str]) -> SourceBinding {
        SourceBinding {
            module: "routes/index.tsx".to_string(),
            component: "Page".to_string(),
            binding: "repo".to_string(),
            source: source.to_string(),
            route: route.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    #[test]
    fn a_matching_binding_validates() {
        assert!(validate_source_bindings(
            &[binding("github", "repo", &["name", "owner"])],
            &registry()
        )
        .is_ok());
    }

    #[test]
    fn a_paramless_route_needs_no_arguments() {
        assert!(validate_source_bindings(&[binding("github", "status", &[])], &registry()).is_ok());
    }

    #[test]
    fn an_unknown_source_is_reported_with_what_is_declared() {
        let err = validate_source_bindings(&[binding("gitlab", "repo", &[])], &registry())
            .expect_err("must fail");
        assert!(matches!(err[0], SourceBindingProblem::UnknownSource { .. }));
        assert!(err[0].to_string().contains("`github`"));
    }

    #[test]
    fn an_unknown_route_names_the_source_that_does_exist() {
        let err = validate_source_bindings(
            &[binding("github", "issues", &["owner", "name"])],
            &registry(),
        )
        .expect_err("must fail");
        assert!(matches!(err[0], SourceBindingProblem::UnknownRoute { .. }));
        let message = err[0].to_string();
        assert!(message.contains("`repo`") && message.contains("`status`"));
    }

    #[test]
    fn a_missing_argument_is_a_build_error() {
        // The failure this module exists for: `github.repo({ owner })` is
        // well-formed TSX and a valid declaration, and would resolve to nothing
        // forever with no error anywhere.
        let err = validate_source_bindings(&[binding("github", "repo", &["owner"])], &registry())
            .expect_err("must fail");
        assert!(matches!(
            err[0],
            SourceBindingProblem::MissingArgument { .. }
        ));
        assert!(err[0].to_string().contains("name"));
    }

    #[test]
    fn an_extra_argument_is_a_build_error() {
        let err = validate_source_bindings(
            &[binding("github", "repo", &["owner", "name", "onwer"])],
            &registry(),
        )
        .expect_err("must fail");
        assert!(err
            .iter()
            .any(|problem| matches!(problem, SourceBindingProblem::UnknownArgument { .. })));
    }

    #[test]
    fn every_problem_is_reported_in_one_pass() {
        let problems = validate_source_bindings(
            &[
                binding("github", "repo", &["owner"]),
                binding("gitlab", "repo", &[]),
            ],
            &registry(),
        )
        .expect_err("must fail");
        assert_eq!(problems.len(), 2, "one boot must report both");
    }

    #[test]
    fn an_empty_registry_reports_every_read_rather_than_passing() {
        // A project that reads sources but declares none must not boot quietly.
        let empty = SourceRegistry::default();
        assert!(validate_source_bindings(&[binding("github", "repo", &[])], &empty).is_err());
    }

    #[test]
    fn no_bindings_and_no_declarations_is_fine() {
        assert!(validate_source_bindings(&[], &SourceRegistry::default()).is_ok());
    }
}
