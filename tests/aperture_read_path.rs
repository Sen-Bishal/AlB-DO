//! APERTURE · A1 — the read path, end to end.
//!
//! Every stage of A1 has unit tests where it lives. This file exists for the
//! thing those cannot show: that the stages agree. The chain is
//!
//! ```text
//!   TSX  →  extractor  →  manifest spec  →  resolver  →  reader  →  topic value
//! ```
//!
//! and each arrow crosses a module boundary where two independently-correct
//! halves could still disagree — a spec whose argument names do not match the
//! template's placeholders, an identity minted one way for render and another
//! for subscribe. `PRISM.md` invariant 5 is the general form of that hazard;
//! these tests are its APERTURE instance.
//!
//! The transpile fold is the one link tested next to its own harness instead
//! (`transforms::shared_slots::tests::fold`), because asserting on it needs the
//! SWC AST rather than this crate's public surface.

use dom_render_compiler::aperture::{
    validate_source_bindings, CountingTransport, EgressMode, RouteDecl, SourceBinding, SourceDecl,
    SourceReader, SourceRegistry, Transport, WireResponse,
};
use dom_render_compiler::manifest::schema::{SourceArgSpec, SourceTopicSpec};
use dom_render_compiler::runtime::resolve_source_topics;
use dom_render_compiler::transforms::shared_slots::{
    extract_shared_slot_hooks, SourceArg, TopicSpec,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const PAGE: &str = r#"
import { useSharedSlot } from "albedo";
import { github } from "albedo/sources";
export default function Component({ params }) {
    const repo = useSharedSlot(github.repo({ owner: params.org, name: "claude-code" }));
    return <div>{repo}</div>;
}
"#;

fn github_sources() -> BTreeMap<String, SourceDecl> {
    let mut routes = BTreeMap::new();
    routes.insert(
        "repo".to_string(),
        RouteDecl {
            path: "/repos/{owner}/{name}".to_string(),
            refresh: Some("60s".to_string()),
            method: None,
        },
    );
    [(
        "github".to_string(),
        SourceDecl {
            base: "https://api.github.com".to_string(),
            auth: None,
            headers: BTreeMap::new(),
            routes,
        },
    )]
    .into_iter()
    .collect()
}

/// The extracted binding, lowered to the manifest shape the same way
/// `CompiledProject::shared_slot_sources_for_entry` lowers it.
fn spec_from_source(source: &str) -> SourceTopicSpec {
    let parsed = dom_render_compiler::runtime::eval::expr::parse_module(
        source,
        Path::new("routes/index.tsx"),
    )
    .expect("parses");
    let function = parsed.functions.get("Component").expect("component");
    let bindings = extract_shared_slot_hooks(function, &parsed.imports).expect("extracts");
    let binding = bindings.first().expect("one binding");
    let TopicSpec::Source {
        source,
        route,
        args,
    } = &binding.spec
    else {
        panic!("expected a source spec, got {:?}", binding.spec);
    };
    SourceTopicSpec {
        binding: binding.binding_name.clone(),
        source: source.clone(),
        route: route.clone(),
        args: args
            .iter()
            .map(|(name, value)| match value {
                SourceArg::Param(param) => SourceArgSpec::Param {
                    name: name.clone(),
                    param: param.clone(),
                },
                SourceArg::Literal(literal) => SourceArgSpec::Literal {
                    name: name.clone(),
                    value: literal.clone(),
                },
            })
            .collect(),
    }
}

/// TSX → extractor → spec → resolver → reader → value, with nothing stubbed but
/// the network.
#[tokio::test]
async fn a_declared_source_read_survives_every_stage() {
    let spec = spec_from_source(PAGE);
    assert_eq!(spec.binding, "repo");

    let transport = Arc::new(CountingTransport::always(WireResponse {
        status: 200,
        body: br#"{"stargazers_count":42}"#.to_vec(),
        etag: Some("\"v1\"".to_string()),
        last_modified: None,
        content_type: Some("application/json".to_string()),
    }));
    let reader = SourceReader::with_transport(
        &github_sources(),
        EgressMode::Dev,
        |_| None,
        transport.clone() as Arc<dyn Transport>,
    )
    .expect("lowers");

    // The render path's params, as they arrive in `props["params"]`.
    let resolved = resolve_source_topics(&[spec], reader.registry(), |name| {
        (name == "org").then_some("anthropics")
    });
    assert_eq!(resolved.len(), 1, "the binding must resolve");
    assert_eq!(resolved[0].binding, "repo");
    assert_eq!(
        resolved[0].topic,
        "aperture:github.repo:owner=anthropics,name=claude-code"
    );

    let read = reader
        .read(&dom_render_compiler::aperture::ResolvedSource {
            topic: resolved[0].topic.clone(),
            url: resolved[0].url.clone(),
            source: resolved[0].source.clone(),
            route: resolved[0].route.clone(),
        })
        .await
        .expect("reads");

    assert_eq!(read.body(), br#"{"stargazers_count":42}"#);
    assert_eq!(
        transport.requests()[0].url,
        "https://api.github.com/repos/anthropics/claude-code"
    );
}

/// The boot check's whole purpose: a call and a declaration that are each
/// individually valid, and wrong together.
#[test]
fn the_boot_check_catches_a_call_the_declaration_cannot_serve() {
    let registry = SourceRegistry::from_declarations(&github_sources(), |_| None).expect("lowers");

    let good = SourceBinding {
        module: "routes/index.tsx".to_string(),
        component: "Component".to_string(),
        binding: "repo".to_string(),
        source: "github".to_string(),
        route: "repo".to_string(),
        args: vec!["name".to_string(), "owner".to_string()],
    };
    assert!(validate_source_bindings(&[good.clone()], &registry).is_ok());

    let missing = SourceBinding {
        args: vec!["owner".to_string()],
        ..good
    };
    let problems = validate_source_bindings(&[missing], &registry).expect_err("must fail");
    assert!(
        problems[0].to_string().contains("name"),
        "the error must name the missing placeholder: {}",
        problems[0]
    );
}

/// Two components reading the same resource with differently-ordered object
/// literals must land on **one** topic — otherwise the coalescing that makes the
/// read side worth having would silently not happen.
#[tokio::test]
async fn two_spellings_of_one_resource_are_one_topic_and_one_request() {
    let forward = spec_from_source(
        r#"
        import { useSharedSlot } from "albedo";
        import { github } from "albedo/sources";
        export default function Component() {
            const repo = useSharedSlot(github.repo({ owner: "anthropics", name: "claude-code" }));
            return <div>{repo}</div>;
        }
        "#,
    );
    let reverse = spec_from_source(
        r#"
        import { useSharedSlot } from "albedo";
        import { github } from "albedo/sources";
        export default function Component() {
            const repo = useSharedSlot(github.repo({ name: "claude-code", owner: "anthropics" }));
            return <div>{repo}</div>;
        }
        "#,
    );

    let transport = Arc::new(CountingTransport::always(WireResponse {
        status: 200,
        body: br#"{"n":1}"#.to_vec(),
        etag: Some("\"v1\"".to_string()),
        last_modified: None,
        content_type: Some("application/json".to_string()),
    }));
    let reader = SourceReader::with_transport(
        &github_sources(),
        EgressMode::Dev,
        |_| None,
        transport.clone() as Arc<dyn Transport>,
    )
    .expect("lowers");

    let one = resolve_source_topics(&[forward], reader.registry(), |_| None);
    let two = resolve_source_topics(&[reverse], reader.registry(), |_| None);
    assert_eq!(one[0].topic, two[0].topic);
    assert_eq!(one[0].url, two[0].url);

    for resolved in [&one[0], &two[0]] {
        reader
            .read(&dom_render_compiler::aperture::ResolvedSource {
                topic: resolved.topic.clone(),
                url: resolved.url.clone(),
                source: resolved.source.clone(),
                route: resolved.route.clone(),
            })
            .await
            .expect("reads");
    }
    assert_eq!(
        transport.calls(),
        1,
        "one resource spelled two ways must cost one request"
    );
}
