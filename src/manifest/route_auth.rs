//! AUTH § 4 · the boot check behind `export const auth = "…"`.
//!
//! [`ManifestBuilder::extract_route_auth`] *records* what a route declared and
//! cannot fail; this *refuses* what does not make sense and stops the boot. The
//! split is the one `partition_by` already uses — the artifact carries what was
//! written, and the boot that would serve it is where a disagreement becomes an
//! error naming the offending file.
//!
//! 🔒 That pairing is what makes the builder's permissive default safe. A route
//! whose `auth` could not be read falls back to
//! [`RouteAuth::Public`](crate::manifest::schema::RouteAuth::Public) in the
//! manifest — but the boot that would serve that manifest does not start, so the
//! wrongly-public route is never reachable.
//!
//! Two things are refused here, and they fail for different reasons:
//!
//! 1. **An unreadable declaration** — `auth` present but not `"public"` /
//!    `"required"`, or not a string literal at all. A typo (`"requried"`) would
//!    otherwise be silently public, which is the worst possible outcome for a
//!    line whose entire purpose is to restrict.
//! 2. **A gate nothing can satisfy** — `auth = "required"` in a project that
//!    declares no auth providers. Every request to that route would be refused
//!    forever, because there is no way to become authenticated. That is a
//!    misconfiguration the author cannot see from either file alone: the route
//!    looks right, the `auth` block looks right, and only the pair is wrong.

use crate::manifest::metadata::auth_from_const_expr;
use crate::runtime::compiled::CompiledProject;

/// Check every route module's `auth` declaration.
///
/// `has_auth_providers` is whether the lowered `auth` block declares anyone who
/// could sign in. Passed in rather than read here because the compiler crate has
/// no view of the server's lowered config, and because the check is exactly a
/// question about the *pair*.
///
/// # Errors
/// Every problem found, sorted by module, so a project with three bad routes is
/// fixed in one pass rather than three boots.
pub fn validate_route_auth(
    project: &CompiledProject,
    has_auth_providers: bool,
) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();

    let mut entries: Vec<_> = project.modules().iter().collect();
    // Sorted so a multi-route failure reads the same on every machine — the
    // module map is a `HashMap`, and an unstable error order makes a build
    // failure look different run to run for no reason.
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    for (entry, module) in entries {
        let Some((_, expr)) = module
            .module_constants
            .iter()
            .find(|(name, _)| name == "auth")
        else {
            continue;
        };

        match auth_from_const_expr(expr) {
            Err(message) => problems.push(format!("{entry}: {message}")),
            Ok(auth) => {
                if !auth.allows_anonymous() && !has_auth_providers {
                    problems.push(format!(
                        "{entry}: declares `auth = \"required\"`, but the `auth` block in \
                         albedo.config.ts declares no providers — nobody can sign in, so every \
                         request to this route would be refused forever. Declare a provider, or \
                         make the route public"
                    ));
                }
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
    use crate::manifest::schema::RouteAuth;

    #[test]
    fn the_two_spellings_parse() {
        assert_eq!(RouteAuth::parse("public"), Ok(RouteAuth::Public));
        assert_eq!(RouteAuth::parse("required"), Ok(RouteAuth::Required));
    }

    /// The failure this whole check exists for: a typo must not read as public.
    #[test]
    fn a_misspelled_value_is_refused_and_lists_the_valid_ones() {
        let err = RouteAuth::parse("requried").expect_err("a typo must not be accepted");
        assert!(err.contains("\"public\""), "{err}");
        assert!(err.contains("\"required\""), "{err}");
        assert!(err.contains("requried"), "the message must quote what was found: {err}");
    }

    #[test]
    fn public_is_the_default_so_routes_written_before_this_field_are_unchanged() {
        assert_eq!(RouteAuth::default(), RouteAuth::Public);
        assert!(RouteAuth::default().allows_anonymous());
    }

    #[test]
    fn required_does_not_allow_anonymous() {
        assert!(!RouteAuth::Required.allows_anonymous());
    }
}
