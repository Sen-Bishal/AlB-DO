//! Adversarial source, declaration and manifest contracts for role gates.

use dom_render_compiler::auth::declare::AuthDeclaration;
use dom_render_compiler::manifest::metadata::auth_from_const_expr;
use dom_render_compiler::manifest::schema::{Admission, RouteAuth};
use dom_render_compiler::manifest::validate_route_auth;
use dom_render_compiler::runtime::compiled::CompiledProject;
use dom_render_compiler::runtime::eval::expr::parse_module;
use std::path::Path;

fn parse_gate(source: &str) -> Result<RouteAuth, String> {
    let module = parse_module(source, Path::new("routes/pay roll-λ.tsx")).expect("parse TSX");
    let (_, expr) = module
        .module_constants
        .iter()
        .find(|(name, _)| name == "auth")
        .expect("auth retained");
    auth_from_const_expr(expr)
}

#[test]
fn unreadable_fragments_never_turn_into_a_smaller_role_requirement() {
    for expression in [
        r#"{ role: ["payroll", , "hr"] }"#,
        r#"{ role: ["payroll", ...["hr"]] }"#,
        r#"{ get role() { return "payroll"; } }"#,
        r#"{ role: "payroll", extra() {} }"#,
        r#"{ ["role"]: "payroll" }"#,
        r#"{ role: "payroll", other: undefined }"#,
        r#"{ role: "payroll", ...{} }"#,
        r#"({ role: "payroll" }, "public")"#,
        r#"true ? { role: "payroll" } : "public""#,
    ] {
        let source = format!("export const auth = {expression};");
        assert!(parse_gate(&source).is_err(), "accepted {source}");
    }
}

#[test]
fn crlf_wrappers_and_escaped_role_names_preserve_the_same_gate() {
    let gate = parse_gate(
        "\r\nexport const auth = (({ role: [\"pay\\u0072oll\", \"hr\", \"hr\"] } as const));\r\n",
    )
    .expect("literal gate");
    assert_eq!(gate, RouteAuth::Roles(vec!["hr".into(), "payroll".into()]));
    assert_eq!(gate.admits(Some(&["hR".into()])), Admission::Forbidden);
    assert_eq!(gate.admits(Some(&["hr".into()])), Admission::Allowed);
}

#[test]
fn duplicate_literal_keys_follow_javascript_but_never_hide_computed_fragments() {
    let gate = parse_gate(r#"export const auth = { role: "hr", role: "payroll" };"#)
        .expect("last literal property wins in JavaScript");
    assert_eq!(gate, RouteAuth::Roles(vec!["payroll".into()]));
    assert_eq!(gate.admits(Some(&["hr".into()])), Admission::Forbidden);
    for expression in [
        r#"{ role: unknownRole, role: "payroll" }"#,
        r#"{ role: "payroll", role: unknownRole }"#,
    ] {
        assert!(parse_gate(&format!("export const auth = {expression};")).is_err());
    }
}

#[test]
fn boot_rejects_empty_computed_and_undeclared_gates_from_real_modules() {
    for (expression, expected) in [
        (r#"{ role: [] }"#, "names no role"),
        (r#"{ role: "finance" }"#, "finance"),
        (r#"{ role: ["payroll", roleName] }"#, "literal"),
    ] {
        let dir = tempfile::tempdir().expect("temp project");
        let routes = dir.path().join("routes");
        std::fs::create_dir(&routes).expect("routes");
        std::fs::write(
            routes.join("pay roll.tsx"),
            format!(
                "export const auth = {expression};\r\nexport default function Payroll() {{ return <p>secret</p>; }}\r\n"
            ),
        )
        .expect("write route");
        let project = CompiledProject::load_from_dir(dir.path()).expect("compile project");
        let errors =
            validate_route_auth(&project, true, &["payroll".into()]).expect_err("boot must refuse");
        let message = errors.join("\n");
        assert!(message.contains(expected), "{message}");
        assert!(message.contains("pay roll.tsx"), "{message}");
    }
}

#[test]
fn declaration_enforces_ascii_and_exact_length_boundaries() {
    let maximum = format!("a{}", "9_-".repeat(10) + "z");
    assert_eq!(maximum.len(), 32);
    let valid: AuthDeclaration = serde_json::from_value(serde_json::json!({
        "providers": { "passkey": {} }, "roles": [maximum]
    }))
    .expect("declaration");
    assert_eq!(
        valid.lower().expect("32 bytes accepted").roles,
        [maximum.clone()]
    );
    for role in [
        format!("{maximum}z"),
        "a,b".into(),
        "a\0b".into(),
        "a\r\nb".into(),
        "ａdmin".into(),
        "équipe".into(),
        "admin ".into(),
        "-admin".into(),
    ] {
        let declaration: AuthDeclaration = serde_json::from_value(serde_json::json!({
            "providers": { "passkey": {} }, "roles": [role]
        }))
        .expect("declaration shape");
        let error = declaration
            .lower()
            .expect_err("invalid name refused")
            .to_string();
        assert!(error.contains(&role), "offending value omitted: {error:?}");
    }
}

#[test]
fn large_literal_role_lists_canonicalize_without_widening_admission() {
    let items = vec![serde_json::Value::String("payroll".into()); 16_384];
    let gate = RouteAuth::from_json(&serde_json::json!({ "role": items })).expect("large list");
    assert_eq!(gate, RouteAuth::Roles(vec!["payroll".into()]));
    assert_eq!(gate.admits(None), Admission::Anonymous);
    assert_eq!(gate.admits(Some(&[])), Admission::Forbidden);
    assert_eq!(gate.admits(Some(&["admin".into()])), Admission::Forbidden);
}

#[test]
fn legacy_and_role_manifest_requirements_round_trip_without_changing_admission() {
    for gate in [
        RouteAuth::Public,
        RouteAuth::Required,
        RouteAuth::Roles(vec!["hr".into(), "payroll".into()]),
    ] {
        let encoded = serde_json::to_string(&gate).expect("encode");
        let restored: RouteAuth = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(restored, gate);
        for held in [None, Some(vec![]), Some(vec!["hr".into()])] {
            assert_eq!(
                restored.admits(held.as_deref()),
                gate.admits(held.as_deref())
            );
        }
    }
    for (old, expected) in [
        (r#""public""#, RouteAuth::Public),
        (r#""required""#, RouteAuth::Required),
    ] {
        assert_eq!(
            serde_json::from_str::<RouteAuth>(old).expect("old manifest"),
            expected
        );
    }
}
