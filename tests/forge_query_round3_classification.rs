//! Adversarial binding and span handoff checks for T-0033 verification round 3.

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::runtime::eval::CompiledProject;
use dom_render_compiler::transforms::forge_queries::classify_project;
use dom_render_compiler::transforms::shared_slot_lists::{
    classify_shared_slot_lists_source, mark_shared_slot_lists, mark_shared_slot_scalars,
    RowProjection, LIST_SLOT_ATTR, SLOT_ATTR,
};
use std::collections::BTreeMap;
use swc_common::{sync::Lrc, FileName, Globals, Mark, SourceMap, GLOBALS};
use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXOpeningElement, Module};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMutWith, VisitWith};

fn declarations() -> BTreeMap<String, CollectionDecl> {
    serde_json::from_str(
        r#"{"messages":{"fields":{"room":"text","body":"text"},"partition_by":"room"}}"#,
    )
    .unwrap()
}

fn parse(source: &str) -> Module {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        FileName::Custom("review.tsx".into()).into(),
        source.to_string(),
    );
    Parser::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        StringInput::from(&*fm),
        None,
    )
    .parse_module()
    .unwrap()
}

#[test]
fn collection_aliases_parens_and_crlf_keep_real_and_shadowed_sites_distinct() {
    let dir = tempfile::tempdir().unwrap();
    for (name, source) in [
        (
            "A.tsx",
            "import { useSharedSlot } from 'albedo';\r\nimport { messages as rows } from 'albedo/forge';\r\nexport default function A({ params }) { const data = useSharedSlot((rows).where({ room: params.id })); return <p>{data.length}</p>; }",
        ),
        (
            "B.tsx",
            "import { useSharedSlot } from 'albedo';\r\nimport { messages as rows } from 'albedo/forge';\r\nexport default function B(rows) { const data = useSharedSlot((rows).where({ room: 'local' })); return <p>{data.length}</p>; }",
        ),
    ] {
        std::fs::write(dir.path().join(name), source).unwrap();
    }
    let result = classify_project(dir.path(), dir.path(), &declarations()).unwrap();
    assert!(result.refusal().is_none(), "{result:?}");
    assert_eq!(result.legacy, 1);
    let project = CompiledProject::load_from_dir(dir.path()).unwrap();
    assert_eq!(
        project
            .component_meta("A.tsx", "A")
            .unwrap()
            .shared_slots
            .len(),
        1
    );
    assert!(project
        .component_meta("B.tsx", "B")
        .unwrap()
        .shared_slots
        .is_empty());
}

#[test]
fn an_aliased_framework_hook_keeps_its_classified_legacy_subscription() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Page.tsx"),
        "import { useSharedSlot as shared } from 'albedo'; import { messages } from 'albedo/forge'; export default function Page() { const rows = shared(messages); return <p>{rows.length}</p>; }",
    )
    .unwrap();
    let result = classify_project(dir.path(), dir.path(), &declarations()).unwrap();
    assert!(result.refusal().is_none(), "{result:?}");
    assert_eq!(result.legacy, 1);
    let project = CompiledProject::load_from_dir(dir.path()).unwrap();
    assert_eq!(
        project
            .component_meta("Page.tsx", "Page")
            .unwrap()
            .shared_slots
            .len(),
        1
    );
}

#[derive(Default)]
struct Anchors(Vec<(String, String)>);

impl Visit for Anchors {
    fn visit_jsx_opening_element(&mut self, element: &JSXOpeningElement) {
        let tag = format!("{:?}", element.name);
        for attr in &element.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(name) = &attr.name {
                    if name.sym == LIST_SLOT_ATTR || name.sym == SLOT_ATTR {
                        self.0.push((tag.clone(), name.sym.to_string()));
                    }
                }
            }
        }
    }
}

#[test]
fn shadowed_collection_result_does_not_inherit_another_functions_list_anchor() {
    let source = r#"import { useSharedSlot } from 'albedo';
import { messages } from 'albedo/forge';
export function Real() {
  const rows = useSharedSlot(messages);
  return <ul>{rows.map(row => <li key={row.id}>{row.body}</li>)}</ul>;
}
export function Local(messages) {
  const rows = useSharedSlot(messages);
  return <ol>{rows.map(row => <li key={row.id}>{row.body}</li>)}</ol>;
}
"#;
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let mut module = parse(source);
        module.visit_mut_with(&mut resolver(Mark::new(), Mark::new(), true));
        mark_shared_slot_lists(&mut module);
        let mut anchors = Anchors::default();
        module.visit_with(&mut anchors);
        assert_eq!(
            anchors.0.len(),
            1,
            "ordinary JS received a FORGE list anchor: {:?}",
            anchors.0
        );
    });
}

#[test]
fn shadowed_collection_result_does_not_inherit_another_functions_scalar_anchor() {
    let source = r#"import { useSharedSlot } from 'albedo';
import { messages } from 'albedo/forge';
export function Real() {
  const value = useSharedSlot(messages);
  return <span>{value}</span>;
}

export function Local(messages) {
  const value = useSharedSlot(messages);
  return <em>{value}</em>;
}
"#;
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let mut module = parse(source);
        module.visit_mut_with(&mut resolver(Mark::new(), Mark::new(), true));
        mark_shared_slot_scalars(&mut module);
        let mut anchors = Anchors::default();
        module.visit_with(&mut anchors);
        assert_eq!(
            anchors.0.len(),
            1,
            "ordinary JS received a FORGE scalar anchor: {:?}",
            anchors.0
        );
    });
}

#[test]
fn source_projection_does_not_merge_a_shadowed_collections_same_named_result() {
    let source = r#"import { useSharedSlot } from 'albedo';
import { messages } from 'albedo/forge';
export function Real() {
  const rows = useSharedSlot(messages);
  return <ul>{rows.map(row => <li key={row.id}>{row.body}</li>)}</ul>;
}
export function Local(messages) {
  const rows = useSharedSlot(messages);
  return <ol>{rows.map(row => <li key={row.id}>{rows.length}</li>)}</ol>;
}
"#;
    let projections = classify_shared_slot_lists_source("Page.tsx", source);
    assert_eq!(projections.get("messages"), Some(&RowProjection::PerRecord));
}
