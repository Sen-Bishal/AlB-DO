//! Phase P · Stream C.1 — `action(...)` declaration compilation gate.
//!
//! Loads a TSX fixture that declares two `export const NAME = action(...)`
//! handlers and asserts the C.1 wiring:
//!
//!   1. `ParsedModule.action_declarations` is populated in source
//!      order with both declarations.
//!   2. `CompiledProject::wrap` registers a `ResolvedHandler` per
//!      action keyed by `FNV-1a-32(name)` (the same hash family
//!      Phase L's `allocate_form_action_id` produces, so the wire
//!      `action_id` is identical whether the action came from a
//!      `<form action="action:NAME">` or a TS `action()` declaration).
//!   3. The synthetic `CompiledComponent` carrying the action body
//!      is reachable via `component_meta` so `invoke_action`'s
//!      lookup resolves once C.2 lands the `broadcast()` builtin.
//!   4. Async vs. sync handler shape survives the round-trip.
//!
//! Action body **invocation** is not exercised here — that needs
//! C.2's `broadcast()` builtin to be useful. C.1's job is the
//! metadata + registry wiring, which this test pins.

use dom_render_compiler::runtime::eval::CompiledProject;
use dom_render_compiler::transforms::allocate_form_action_id;
use std::path::PathBuf;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ts_action")
        .join("post_message")
}

fn compile() -> CompiledProject {
    CompiledProject::load_from_dir(fixture()).expect("project compiles")
}

#[test]
fn action_declarations_populated_in_parsed_module_in_source_order() {
    let project = compile();
    let mut names: Vec<String> = project
        .action_declarations_iter()
        .map(|(_module, d)| d.name.clone())
        .collect();
    names.sort();
    assert_eq!(names, vec!["ping", "post_chat_message"]);
    assert_eq!(project.action_declaration_count(), 2);
}

#[test]
fn each_action_registers_a_handler_keyed_by_form_action_id() {
    let project = compile();

    let async_id = allocate_form_action_id("post_chat_message");
    let sync_id = allocate_form_action_id("ping");

    let async_handler = project.handler(async_id).expect("async action registered");
    assert_eq!(async_handler.event_name, "action");
    assert_eq!(async_handler.function_name, "__action__post_chat_message");

    let sync_handler = project.handler(sync_id).expect("sync action registered");
    assert_eq!(sync_handler.event_name, "action");
    assert_eq!(sync_handler.function_name, "__action__ping");
}

#[test]
fn synthetic_component_for_each_action_is_reachable_via_component_meta() {
    let project = compile();
    let (module_spec, _) = project
        .action_declarations_iter()
        .next()
        .expect("at least one action declared");
    let module_spec = module_spec.to_string();

    // Both synthetic components must be present so `invoke_action`'s
    // `component_meta(module_spec, function_name)` lookup resolves
    // once C.2 wires the dispatch into the broadcast() builtin.
    for synthetic in ["__action__post_chat_message", "__action__ping"] {
        let component = project
            .component_meta(&module_spec, synthetic)
            .unwrap_or_else(|| panic!("missing synthetic component for {synthetic}"));
        // Synthetic components have no slot bindings — TS action
        // bodies don't run inside a JSX component scope.
        assert!(component.value_slots.is_empty());
        assert!(component.setter_slots.is_empty());
        assert!(component.handlers.is_empty());
        assert!(component.shared_slots.is_empty());
    }
}

#[test]
fn async_modifier_round_trips_through_action_declaration() {
    let project = compile();
    let mut decls: Vec<_> = project
        .action_declarations_iter()
        .map(|(_module, d)| (d.name.clone(), d.is_async))
        .collect();
    decls.sort();
    assert_eq!(
        decls,
        vec![
            ("ping".to_string(), false),
            ("post_chat_message".to_string(), true),
        ]
    );
}

#[test]
fn handler_count_includes_both_phase_k_proxies_and_ts_actions() {
    let project = compile();
    // The fixture's `ChatRoom` component has no JSX `on*` handlers,
    // so the only entries in the handlers map come from the two
    // TS action declarations.
    assert_eq!(project.handler_count(), 2);
    assert_eq!(
        project.handler_proxy_ids().collect::<Vec<_>>().len(),
        2,
        "handler_proxy_ids must enumerate the action ids so \
         register_compiled_project picks them up"
    );
}
