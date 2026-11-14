//! Classification-to-consumer agreement for legacy query bindings.

use std::collections::BTreeMap;

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::runtime::eval::CompiledProject;
use dom_render_compiler::transforms::forge_queries::classify_project;
use dom_render_compiler::transforms::shared_slot_lists::{
    mark_shared_slot_lists, mark_shared_slot_scalars, LIST_SLOT_ATTR, SLOT_ATTR,
};
use dom_render_compiler::transforms::shared_slots::{
    rewrite_shared_slot_topic_args, KeySource, TopicSpec,
};
use swc_common::{sync::Lrc, FileName, Globals, Mark, SourceMap, GLOBALS};
use swc_ecma_ast::{CallExpr, Callee, Expr, JSXAttrName, JSXAttrOrSpread, JSXOpeningElement, Lit};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMutWith, VisitWith};

fn declarations() -> BTreeMap<String, CollectionDecl> {
    serde_json::from_str(
        r#"{"messages":{"fields":{"room":"text","body":"text"},"partition_by":"room"}}"#,
    )
    .unwrap()
}

fn partition_binding(parameter: &str, key: &str, expected: KeySource) {
    let dir = tempfile::tempdir().unwrap();
    let source = format!(
        "import {{ useSharedSlot }} from 'albedo';\nimport {{ messages }} from 'albedo/forge';\nexport default function Page({parameter}) {{ const rows = useSharedSlot(messages.where({{ room: {key} }})); return <p>{{rows.length}}</p>; }}"
    );
    std::fs::write(dir.path().join("Page.tsx"), source).unwrap();
    let classified = classify_project(dir.path(), dir.path(), &declarations()).unwrap();
    assert!(classified.refusal().is_none(), "{classified:?}");
    assert_eq!(classified.legacy, 1);
    let project = CompiledProject::load_from_dir(dir.path())
        .expect("a partition accepted as legacy must be consumable by the compiled extractor");
    let meta = project.component_meta("Page.tsx", "Page").unwrap();
    assert_eq!(meta.shared_slots.len(), 1);
    assert_eq!(
        meta.shared_slots[0].spec,
        TopicSpec::Partition {
            collection: "messages".into(),
            column: "room".into(),
            key: expected,
        }
    );
}

#[test]
fn an_aliased_route_prop_keeps_its_legacy_partition_key() {
    partition_binding(
        "{ params: route }",
        "route.id",
        KeySource::Param("id".into()),
    );
}

#[test]
fn a_whole_props_binding_keeps_its_legacy_partition_key() {
    partition_binding("props", "props.params.id", KeySource::Param("id".into()));
}

#[test]
fn an_aliased_principal_prop_keeps_its_legacy_identity_key() {
    partition_binding("{ user: actor }", "actor.id", KeySource::Identity);
}

#[derive(Default)]
struct HookArguments(Vec<(String, bool)>);

impl Visit for HookArguments {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Callee::Expr(callee) = &call.callee {
            if let Expr::Ident(name) = &**callee {
                if matches!(name.sym.as_ref(), "first" | "second") {
                    self.0.push((
                        name.sym.to_string(),
                        matches!(call.args[0].expr.as_ref(), Expr::Lit(Lit::Str(text)) if text.value == *"messages"),
                    ));
                }
            }
        }
        call.visit_children_with(self);
    }
}

#[test]
fn every_imported_alias_of_the_hook_has_its_classified_forge_argument_rewritten() {
    let source = "import { useSharedSlot as first, useSharedSlot as second } from 'albedo';\nimport { messages } from 'albedo/forge';\nexport function Page() { const a = first(messages); const b = second(messages); return <p>{a.length + b.length}</p>; }";
    let cm: Lrc<SourceMap> = Default::default();
    let file = cm.new_source_file(
        FileName::Custom("Page.tsx".into()).into(),
        source.to_string(),
    );
    let mut module = Parser::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        StringInput::from(&*file),
        None,
    )
    .parse_module()
    .unwrap();
    GLOBALS.set(&Globals::new(), || {
        module.visit_mut_with(&mut resolver(Mark::new(), Mark::new(), true));
        rewrite_shared_slot_topic_args(&mut module);
    });
    let mut arguments = HookArguments::default();
    module.visit_with(&mut arguments);
    assert_eq!(
        arguments.0,
        [("first".into(), true), ("second".into(), true)]
    );
}

#[derive(Default)]
struct AnchorCounts {
    scalar: usize,
    list: usize,
}

impl Visit for AnchorCounts {
    fn visit_jsx_opening_element(&mut self, element: &JSXOpeningElement) {
        for attr in &element.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(name) = &attr.name {
                    if name.sym == SLOT_ATTR {
                        self.scalar += 1;
                    } else if name.sym == LIST_SLOT_ATTR {
                        self.list += 1;
                    }
                }
            }
        }
        element.visit_children_with(self);
    }
}

fn anchors_for_aliases(render: &str, list: bool) -> AnchorCounts {
    let source = format!(
        "import {{ useSharedSlot as first, useSharedSlot as second }} from 'albedo';\nimport {{ messages }} from 'albedo/forge';\nexport function Page() {{ const a = first(messages); const b = second(messages); return {render}; }}"
    );
    let cm: Lrc<SourceMap> = Default::default();
    let file = cm.new_source_file(FileName::Custom("Page.tsx".into()).into(), source);
    let mut module = Parser::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        StringInput::from(&*file),
        None,
    )
    .parse_module()
    .unwrap();
    GLOBALS.set(&Globals::new(), || {
        module.visit_mut_with(&mut resolver(Mark::new(), Mark::new(), true));
        if list {
            mark_shared_slot_lists(&mut module);
        } else {
            mark_shared_slot_scalars(&mut module);
        }
    });
    let mut counts = AnchorCounts::default();
    module.visit_with(&mut counts);
    counts
}

#[test]
fn every_imported_alias_of_the_hook_receives_a_scalar_anchor() {
    let anchors = anchors_for_aliases("<div><span>{a}</span><em>{b}</em></div>", false);
    assert_eq!(
        anchors.scalar, 2,
        "both classified legacy hook results need scalar anchors"
    );
    assert_eq!(anchors.list, 0);
}

#[test]
fn every_imported_alias_of_the_hook_receives_a_list_anchor() {
    let anchors = anchors_for_aliases(
        "<div><ul>{a.map(row => <li key={row.id}>{row.body}</li>)}</ul><ol>{b.map(row => <li key={row.id}>{row.body}</li>)}</ol></div>",
        true,
    );
    assert_eq!(
        anchors.list, 2,
        "both classified legacy hook results need list anchors"
    );
    assert_eq!(anchors.scalar, 0);
}
