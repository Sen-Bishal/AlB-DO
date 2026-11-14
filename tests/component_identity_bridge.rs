//! T-0044 · `CompiledProject::module_spec_for` finds a component by module
//! path *and* name, so two modules exporting `Page` each resolve to their own
//! spec; the name-only lookup refuses to guess when the name is shared.

use dom_render_compiler::runtime::CompiledProject;
use std::fs;

const LIVE: &str = r#"import { useSharedSlot } from "albedo";
export default function Page() {
  const rows = useSharedSlot("guestbook");
  return <ul>{rows.map((r) => <li key={r.id}>{r.author}</li>)}</ul>;
}
"#;

const STILL: &str = r#"export default function Page() {
  return <p>still</p>;
}
"#;

#[test]
fn same_named_components_resolve_to_their_own_module() {
    let dir = tempfile::tempdir().unwrap();
    let routes = dir.path().join("routes");
    fs::create_dir_all(routes.join("b")).unwrap();
    fs::write(routes.join("live.tsx"), LIVE).unwrap();
    fs::write(routes.join("still.tsx"), STILL).unwrap();
    // Suffix trap: `a.tsx` is a suffix of `…/b/a.tsx` as a string, not as a path.
    fs::write(routes.join("a.tsx"), STILL).unwrap();
    fs::write(routes.join("b").join("a.tsx"), STILL).unwrap();

    let project = CompiledProject::load_from_dir(dir.path()).expect("project compiles");
    let abs = |rel: &str| dir.path().join(rel).to_string_lossy().into_owned();

    let live = project
        .module_spec_for(&abs("routes/live.tsx"), "Page")
        .expect("live resolves");
    let still = project
        .module_spec_for(&abs("routes/still.tsx"), "Page")
        .expect("still resolves");
    assert_ne!(live, still);
    assert!(live.replace('\\', "/").ends_with("live.tsx"), "{live}");
    assert!(still.replace('\\', "/").ends_with("still.tsx"), "{still}");

    // Each spec carries its own shared-slot metadata — the thing the render
    // plan used to borrow from the wrong module.
    assert_eq!(
        project.shared_slot_topics_for_entry(live),
        vec!["guestbook".to_string()]
    );
    assert!(project.shared_slot_topics_for_entry(still).is_empty());

    let nested = project
        .module_spec_for(&abs("routes/b/a.tsx"), "Page")
        .expect("nested resolves");
    assert!(nested.replace('\\', "/").ends_with("b/a.tsx"), "{nested}");
    let top = project
        .module_spec_for(&abs("routes/a.tsx"), "Page")
        .expect("top resolves");
    assert!(!top.replace('\\', "/").ends_with("b/a.tsx"), "{top}");

    // Windows spelling: backslashes and different case still match.
    #[cfg(windows)]
    {
        let shouty = abs("routes/live.tsx")
            .replace('/', "\\")
            .to_ascii_uppercase();
        assert_eq!(project.module_spec_for(&shouty, "Page"), Some(live));
    }

    // The name-only bridge must not guess between modules.
    assert_eq!(project.module_spec_for_component("Page"), None);
}

#[test]
fn module_identity_rejects_unknown_names_and_partial_path_suffixes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("routes")).unwrap();
    fs::write(dir.path().join("routes/a.tsx"), STILL).unwrap();
    let project = CompiledProject::load_from_dir(dir.path()).unwrap();
    for path in ["", "routes/ba.tsx", "notroutes/a.tsx", "routes/a.tsx.bak"] {
        assert_eq!(project.module_spec_for(path, "Page"), None, "{path}");
    }
    assert_eq!(project.module_spec_for("routes/a.tsx", "Missing"), None);
    assert_eq!(project.module_spec_for("routes/a.tsx", "page"), None);
    assert!(project.module_spec_for_component("Page").is_some());
    assert_eq!(project.module_spec_for_component("Missing"), None);
}

#[test]
fn windows_extended_paths_spaces_and_crlf_preserve_module_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let routes = dir.path().join("routes/space dir");
    fs::create_dir_all(&routes).unwrap();
    fs::write(routes.join("live.tsx"), LIVE.replace('\n', "\r\n")).unwrap();
    fs::write(routes.join("still.tsx"), STILL.replace('\n', "\r\n")).unwrap();
    let project = CompiledProject::load_from_dir(dir.path()).unwrap();
    let live_path = dir.path().join("routes/space dir/live.tsx");
    let live = project
        .module_spec_for(&live_path.to_string_lossy(), "Page")
        .unwrap();
    for spelling in [
        r"\\?\C:\app space\routes\space dir\live.tsx",
        r"\\?\UNC\server\share\routes\space dir\live.tsx",
    ] {
        assert_eq!(project.module_spec_for(spelling, "Page"), Some(live));
    }
    assert_eq!(
        project.shared_slot_topics_for_entry(live),
        vec!["guestbook".to_string()]
    );
    let still = project
        .module_spec_for(r"C:\app space\routes\space dir\still.tsx", "Page")
        .unwrap();
    assert!(project.shared_slot_topics_for_entry(still).is_empty());
}

#[test]
fn many_same_named_components_resolve_consistently_under_concurrent_lookup() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..96 {
        let routes = dir.path().join(format!("routes/group-{index}"));
        fs::create_dir_all(&routes).unwrap();
        fs::write(
            routes.join("page.tsx"),
            if index % 2 == 0 { LIVE } else { STILL },
        )
        .unwrap();
    }
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let root = dir.path();
            scope.spawn(move || {
                // Each runtime owns its compiled AST; independent loads also
                // vary HashMap order, as fresh server processes do.
                let project = CompiledProject::load_from_dir(root).unwrap();
                for index in (worker..96).step_by(8) {
                    let path = root.join(format!("routes/group-{index}/page.tsx"));
                    for _ in 0..8 {
                        let spec = project
                            .module_spec_for(&path.to_string_lossy(), "Page")
                            .unwrap();
                        assert!(
                            spec.replace('\\', "/")
                                .ends_with(&format!("group-{index}/page.tsx")),
                            "{spec}"
                        );
                        let topics = project.shared_slot_topics_for_entry(spec);
                        assert_eq!(
                            topics,
                            if index % 2 == 0 {
                                vec!["guestbook".to_string()]
                            } else {
                                Vec::new()
                            }
                        );
                    }
                }
                assert_eq!(project.module_spec_for_component("Page"), None);
            });
        }
    });
}

// Case-sensitive hosts can contain both files. Their identities must remain
// distinct even though Windows legitimately folds ASCII path case.
#[cfg(not(windows))]
#[test]
fn case_sensitive_module_paths_do_not_alias() {
    let dir = tempfile::tempdir().unwrap();
    let routes = dir.path().join("routes");
    fs::create_dir_all(&routes).unwrap();
    fs::write(routes.join("page.tsx"), LIVE).unwrap();
    fs::write(routes.join("Page.tsx"), STILL).unwrap();
    // macOS also runs this test, often on a case-insensitive volume.
    if fs::read_to_string(routes.join("page.tsx")).unwrap() != LIVE {
        eprintln!("case-distinct module fixture requires a case-sensitive volume");
        return;
    }
    let project = CompiledProject::load_from_dir(dir.path()).unwrap();
    let lower = project
        .module_spec_for(&routes.join("page.tsx").to_string_lossy(), "Page")
        .unwrap();
    let upper = project
        .module_spec_for(&routes.join("Page.tsx").to_string_lossy(), "Page")
        .unwrap();
    assert_ne!(lower, upper, "case-sensitive module identities were merged");
    assert_eq!(
        project.shared_slot_topics_for_entry(lower),
        vec!["guestbook".to_string()]
    );
    assert!(project.shared_slot_topics_for_entry(upper).is_empty());
}
