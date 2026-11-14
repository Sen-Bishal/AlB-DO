//! T-0044: adding a component key must preserve old unique-name node JSON.

use dom_render_compiler::manifest::schema::{qualified_component_name, TierBNode, TierCNode};

#[test]
fn old_unique_name_nodes_round_trip_with_identical_json_bytes() {
    let old_b = r#"{"component_id":"Room","placeholder_id":"b-1","render_fn":"render::Room","static_props":null,"dynamic_prop_keys":[],"data_deps":[],"tier_a_children":[],"position":{"parent_placeholder":null,"slot":"default","order":0},"timeout_ms":5000,"fallback_html":null,"initial_html":null,"initial_opcode_frame":[]}"#;
    let b: TierBNode = serde_json::from_str(old_b).unwrap();
    assert_eq!(b.component_key, None);
    assert_eq!(serde_json::to_string(&b).unwrap(), old_b);
    let old_c = r#"{"component_id":"Counter","placeholder_id":"c-1","bundle_path":"/counter.js","initial_props":null,"hydration_mode":"immediate","position":{"parent_placeholder":null,"slot":"default","order":0},"side_effects":false}"#;
    let c: TierCNode = serde_json::from_str(old_c).unwrap();
    assert_eq!(c.component_key, None);
    assert_eq!(serde_json::to_string(&c).unwrap(), old_c);
    assert_eq!(qualified_component_name("Room", None), "Room");
}

#[test]
fn duplicate_keys_survive_json_including_zero_and_u64_max() {
    let old_b = serde_json::json!({
        "component_id": "Page", "placeholder_id": "b-1", "render_fn": "render::Page",
        "static_props": null, "dynamic_prop_keys": [], "data_deps": [], "tier_a_children": [],
        "position": {"parent_placeholder": null, "slot": "default", "order": 0},
        "timeout_ms": 5000, "fallback_html": null
    });
    for key in [0, 1, u64::MAX] {
        let mut node: TierBNode = serde_json::from_value(old_b.clone()).unwrap();
        node.component_key = Some(key);
        let value = serde_json::to_value(&node).unwrap();
        assert_eq!(value["component_key"], serde_json::json!(key));
        let decoded: TierBNode = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.component_key, Some(key));
        assert_eq!(
            qualified_component_name("Page", decoded.component_key),
            format!("Page#{key}")
        );
    }
    for invalid in [
        serde_json::json!(-1),
        serde_json::json!("1"),
        serde_json::json!(1.5),
    ] {
        let mut value = old_b.clone();
        value["component_key"] = invalid;
        assert!(serde_json::from_value::<TierBNode>(value).is_err());
    }
}
