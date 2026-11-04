//! Dispatch identities must never select an arbitrary module on a collision.
use dom_render_compiler::runtime::CompiledProject;
use dom_render_compiler::transforms::form::allocate_form_action_id;

fn project_with_actions(first: &str, second: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (file, name) in [("first.tsx", first), ("second.tsx", second)] {
        std::fs::write(dir.path().join(file), format!(
            "import {{ action }} from 'albedo'; export const {name} = action(() => {{ return {{ ok: true }}; }});"
        )).unwrap();
    }
    dir
}

fn refusal(first: &str, second: &str) -> String {
    let dir = project_with_actions(first, second);
    match CompiledProject::load_from_dir(dir.path()) {
        Ok(_) => panic!("ambiguous actions {first} and {second} were accepted"),
        Err(err) => err.to_string(),
    }
}

#[test]
fn duplicate_action_names_are_refused_with_both_modules() {
    let error = refusal("save", "save");
    assert!(
        error.contains("first.tsx") && error.contains("second.tsx"),
        "{error}"
    );
    assert!(error.contains("save"), "{error}");
}

#[test]
fn different_action_names_with_the_same_wire_hash_are_refused() {
    // An actual FNV-1a-32 collision, not a mocked allocator.
    assert_eq!(
        allocate_form_action_id("action_38958"),
        allocate_form_action_id("action_323656")
    );
    let error = refusal("action_38958", "action_323656");
    for label in ["first.tsx", "second.tsx", "action_38958", "action_323656"] {
        assert!(error.contains(label), "missing {label}: {error}");
    }
}

#[test]
fn distinct_action_ids_remain_available() {
    let dir = project_with_actions("save_first", "save_second");
    let project = CompiledProject::load_from_dir(dir.path()).unwrap();
    let ids: Vec<_> = project.handler_proxy_ids().collect();
    assert_eq!(project.action_declaration_count(), 2);
    assert!(ids.contains(&allocate_form_action_id("save_first")));
    assert!(ids.contains(&allocate_form_action_id("save_second")));
}
