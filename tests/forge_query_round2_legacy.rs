//! Verify that legacy extraction respects the classifier's binding identity.

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::runtime::eval::CompiledProject;
use dom_render_compiler::transforms::forge_queries::classify_project;
use std::collections::BTreeMap;

#[test]
fn a_shadowed_collection_accepted_by_classification_is_not_a_forge_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"import { useSharedSlot } from 'albedo';
import { messages } from 'albedo/forge';
export default function Page(messages, { params }) {
  const rows = useSharedSlot(messages.where({ room: params.id }));
  return <p>{rows.length}</p>;
}
"#;
    std::fs::write(dir.path().join("Page.tsx"), source).unwrap();
    let decls: BTreeMap<String, CollectionDecl> = serde_json::from_str(
        r#"{"messages":{"fields":{"room":"text","body":"text"},"partition_by":"room"}}"#,
    )
    .unwrap();
    let classified = classify_project(dir.path(), dir.path(), &decls).unwrap();
    assert!(classified.refusal().is_none(), "{classified:?}");
    assert_eq!(classified.legacy, 0, "the receiver is the parameter");

    let project = CompiledProject::load_from_dir(dir.path()).unwrap();
    let meta = project.component_meta("Page.tsx", "Page").unwrap();
    assert!(
        meta.shared_slots.is_empty(),
        "legacy extraction invented a subscription for ordinary JS: {:?}",
        meta.shared_slots
    );
}
