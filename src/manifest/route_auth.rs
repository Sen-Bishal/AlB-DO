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
//!    R1a adds the same pair-check for roles: `auth = { role: "finance" }` in a
//!    project whose `auth.roles` does not declare `finance`.

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
    declared_roles: &[String],
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
                        "{entry}: declares a gated `auth`, but the `auth` block in \
                         albedo.config.ts declares no providers — nobody can sign in, so every \
                         request to this route would be refused forever. Declare a provider, or \
                         make the route public"
                    ));
                } else if let Err(message) = auth.check_declared(declared_roles) {
                    problems.push(format!("{entry}: {message}"));
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

    use crate::manifest::schema::Admission;

    fn roles(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    /// R1a · the object forms, one role or any of several, sorted and
    /// de-duplicated so a manifest is the same whatever order the author wrote.
    #[test]
    fn a_role_object_parses() {
        assert_eq!(
            RouteAuth::from_json(&serde_json::json!({ "role": "payroll" })),
            Ok(RouteAuth::Roles(roles(&["payroll"])))
        );
        assert_eq!(
            RouteAuth::from_json(&serde_json::json!({ "role": ["payroll", "hr", "hr"] })),
            Ok(RouteAuth::Roles(roles(&["hr", "payroll"])))
        );
    }

    /// Every shape that would otherwise be silently public, or a gate nothing can
    /// pass, is refused with the value quoted.
    #[test]
    fn a_malformed_role_object_is_refused() {
        for bad in [
            serde_json::json!({ "role": [] }),
            serde_json::json!({ "role": "" }),
            serde_json::json!({ "role": "Payroll" }),
            serde_json::json!({ "role": 7 }),
            serde_json::json!({ "roles": "payroll" }),
            serde_json::json!({ "role": "payroll", "extra": true }),
            serde_json::json!({}),
        ] {
            assert!(RouteAuth::from_json(&bad).is_err(), "{bad} must not parse");
        }
    }

    /// The one rule every door applies.
    #[test]
    fn admission_distinguishes_strangers_from_the_unentitled() {
        let gate = RouteAuth::Roles(roles(&["hr", "payroll"]));
        assert_eq!(gate.admits(None), Admission::Anonymous);
        assert_eq!(gate.admits(Some(&roles(&[]))), Admission::Forbidden);
        assert_eq!(gate.admits(Some(&roles(&["admin"]))), Admission::Forbidden);
        assert_eq!(gate.admits(Some(&roles(&["payroll"]))), Admission::Allowed);

        assert_eq!(RouteAuth::Required.admits(None), Admission::Anonymous);
        assert_eq!(RouteAuth::Required.admits(Some(&roles(&[]))), Admission::Allowed);
        assert_eq!(RouteAuth::Public.admits(None), Admission::Allowed);
    }

    /// A route naming a role the `auth` block does not declare is a typo until
    /// proven otherwise, and a gate nobody can ever pass.
    #[test]
    fn an_undeclared_role_is_refused() {
        let gate = RouteAuth::Roles(roles(&["payroll", "finance"]));
        let err = gate
            .check_declared(&roles(&["admin", "payroll"]))
            .expect_err("finance is not declared");
        assert!(err.contains("finance"), "{err}");
        assert!(RouteAuth::Roles(roles(&["payroll"]))
            .check_declared(&roles(&["admin", "payroll"]))
            .is_ok());
        assert!(RouteAuth::Required.check_declared(&[]).is_ok());
    }

    /// Manifests written before roles existed still load, and the new variant
    /// round-trips.
    #[test]
    fn the_manifest_spelling_round_trips_and_old_manifests_still_load() {
        assert_eq!(
            serde_json::from_str::<RouteAuth>(r#""required""#).unwrap(),
            RouteAuth::Required
        );
        let gate = RouteAuth::Roles(roles(&["payroll"]));
        let text = serde_json::to_string(&gate).unwrap();
        assert_eq!(text, r#"{"roles":["payroll"]}"#);
        assert_eq!(serde_json::from_str::<RouteAuth>(&text).unwrap(), gate);
    }
}
