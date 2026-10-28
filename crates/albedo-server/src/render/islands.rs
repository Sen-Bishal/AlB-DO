//! Islands rendered from **this request's** props.
//!
//! # The defect
//!
//! Every Tier-C island used to be rendered once, at boot, from the props the
//! build captured — and the build has no request. Three shapes served the same
//! island to every URL and every visitor (reproduced 2026-10-15 on a fresh
//! `albedo init` app):
//!
//! | shape | `/x/7` | `/x/zz` |
//! |---|---|---|
//! | `[id].tsx` holding `useState` (the page *is* the island) | `counter ` | `counter ` |
//! | `<Picker id={params.id} />` under a per-request page | `picker ` | `picker ` |
//! | `<Who name={user.id} />` under a page reading `user`, signed in | `who anon` | — |
//!
//! The page around the island was right; only the island was frozen. Nothing
//! failed: the build was green, every response 200.
//!
//! # Where an island's props come from on a request
//!
//! - **From its parent's render**, when the parent is rendered per request
//!   (Tier B). The island's client-reference stub now hands its `props` to
//!   `__albedo_island_placeholder`, which writes them onto the placeholder as
//!   `data-albedo-props`. This pass reads them off, renders the island, and
//!   replaces the whole placeholder — so the attribute never reaches a browser.
//! - **From the request itself**, for the props the island reads on its own
//!   (`TierCNode::dynamic_prop_keys`: `params` on a `[slug]` page that is
//!   itself an island, `user` on one that names the principal) — resolved by
//!   the same [`RequestContext::resolve`] a Tier-B node's are.
//!
//! An island with neither keeps its boot render, which is still exact for it.
//!
//! # How it is rendered again
//!
//! The way boot served it ([`IslandRender`]): a serve-wired island rebuilds its
//! binding payload from the compiled project, and its `boot(…)` script replaces
//! the boot-time one; a hydrated (A3) island renders through the engine pool and
//! carries its props on its root as `data-albedo-props`, which the client
//! prefers to the payload's boot-time props.

use crate::render::tier_b::{RequestContext, TierBRenderRegistry};
use crate::renderer_runtime::{
    is_serve_wireable, mark_island_html, reactive_install_script, IslandRender, RequestIsland,
    RouteHydration,
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

/// The attribute a client-reference stub writes the parent's props into. Only
/// ever on a Tier-C placeholder, and only between a render and this pass.
const PROPS_ATTR: &str = " data-albedo-props=\"";

/// What the request path needs to render an island again.
#[derive(Clone, Default)]
pub struct IslandServices {
    /// Rebuilds a serve-wired island's binding payload. `None` (tests, a
    /// manifest served without its source) leaves serve-wired islands on their
    /// boot render.
    pub project: Option<Arc<dom_render_compiler::runtime::CompiledProject>>,
}

/// One island placeholder found in rendered markup.
struct Occurrence {
    start: usize,
    end: usize,
    placeholder_id: String,
    /// `None` — no attribute; `Some(None)` — an attribute that did not parse.
    carried: Option<Option<Value>>,
}

/// Render every island in `html` whose props this request decides, and replace
/// its placeholder with the result.
///
/// Returns the markup and the binding-mode installs this request owes, keyed
/// by placeholder id, for [`RouteHydration::closing_scripts_for`]. Placeholders
/// left untouched are plain and filled from boot by `fill_server_placeholders`.
pub async fn render_request_islands(
    html: String,
    hydration: Option<&RouteHydration>,
    ctx: &RequestContext,
    services: &IslandServices,
    registry: &(dyn TierBRenderRegistry + Send + Sync),
) -> (String, Vec<(String, String)>) {
    let Some(hydration) = hydration.filter(|h| !h.islands.is_empty()) else {
        return (strip_carried_props(html), Vec::new());
    };

    let mut occurrences = find_placeholders(&html, hydration);
    occurrences.sort_by_key(|occurrence| occurrence.start);

    let mut renders = FuturesUnordered::new();
    for (index, occurrence) in occurrences.iter().enumerate() {
        let island = &hydration.islands[&occurrence.placeholder_id];
        let Some(props) = request_props(island, occurrence.carried.as_ref(), ctx) else {
            continue;
        };
        renders.push(async move {
            let rendered = render_island(island, props, services, registry).await;
            (index, rendered)
        });
    }

    let mut replacements: HashMap<usize, String> = HashMap::new();
    let mut installs = Vec::new();
    while let Some((index, rendered)) = renders.next().await {
        let occurrence = &occurrences[index];
        match rendered {
            Ok((markup, install)) => {
                replacements.insert(index, markup);
                if let Some(install) = install {
                    installs.push((occurrence.placeholder_id.clone(), install));
                }
            }
            // The boot render is what the page had before this pass existed, so
            // it is the fallback — loudly, because it is the build's value.
            Err(reason) => error!(
                target: "albedo.render",
                island = %occurrence.placeholder_id,
                error = %reason,
                "island failed to render for this request; serving its boot render"
            ),
        }
    }

    // Back to front, so earlier offsets stay valid. Every occurrence that
    // carried props is rewritten — rendered, or reduced to the plain
    // placeholder the boot fill matches — so the attribute never ships.
    let mut out = html;
    for (index, occurrence) in occurrences.iter().enumerate().rev() {
        let replacement = match replacements.remove(&index) {
            Some(markup) => markup,
            None if occurrence.carried.is_some() => plain_placeholder(&occurrence.placeholder_id),
            None => continue,
        };
        out.replace_range(occurrence.start..occurrence.end, &replacement);
    }
    installs.sort();
    (strip_carried_props(out), installs)
}

/// The props this request renders `island` from, or `None` when its boot
/// render is already right for it.
fn request_props(
    island: &RequestIsland,
    carried: Option<&Option<Value>>,
    ctx: &RequestContext,
) -> Option<Value> {
    let boot = match &island.boot_props {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let mut props = match carried {
        Some(Some(Value::Object(map))) => map.clone(),
        // An attribute that did not parse is a bug in the stub, not a reason
        // to serve the build's value: render from boot props, and say so.
        Some(_) => {
            error!(
                target: "albedo.render",
                "island placeholder carried props that did not parse; rendering from boot props"
            );
            boot.clone()
        }
        None => boot.clone(),
    };
    let carried_differs = matches!(carried, Some(Some(_))) && props != boot;
    if !carried_differs && island.dynamic_prop_keys.is_empty() {
        return None;
    }
    for key in &island.dynamic_prop_keys {
        props.insert(key.clone(), ctx.resolve(key).unwrap_or(Value::Null));
    }
    Some(Value::Object(props))
}

/// Render one island from `props`, the way boot served it. Returns its markup
/// and, for a serve-wired island, the `boot(…)` script that wires it.
async fn render_island(
    island: &RequestIsland,
    props: Value,
    services: &IslandServices,
    registry: &(dyn TierBRenderRegistry + Send + Sync),
) -> Result<(String, Option<String>), String> {
    match &island.render {
        IslandRender::Binding { entry } => {
            let project = services
                .project
                .clone()
                .ok_or("no compiled project to rebuild its binding payload from")?;
            let entry = entry.clone();
            let payload = tokio::task::spawn_blocking(move || {
                use dom_render_compiler::runtime::eval::SessionSlotView;
                use dom_render_compiler::runtime::slot_store::SlotStore;
                use dom_render_compiler::runtime::SessionId;
                let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
                project.build_reactive_payload(&entry, &props, &slots)
            })
            .await
            .map_err(|err| err.to_string())?
            .map_err(|err| err.to_string())?;
            // Eligibility was decided at boot from the boot props; a request's
            // props that change the island's structure cannot be wired by
            // bindings that were not emitted.
            if !is_serve_wireable(&payload) {
                return Err("these props make it ineligible for binding mode".to_string());
            }
            let install = reactive_install_script(&payload)
                .ok_or("its binding payload did not serialise")?;
            Ok((payload.html, Some(install)))
        }
        IslandRender::Hydrate {
            plan_key,
            component_id,
        } => {
            let html = registry
                .call(plan_key, &props, &HashMap::new())
                .await
                .map_err(|err| err.to_string())?;
            Ok((mark_island_html(&html, *component_id, Some(&props)), None))
        }
    }
}

/// Every placeholder in `html` for an island this route can re-render.
fn find_placeholders(html: &str, hydration: &RouteHydration) -> Vec<Occurrence> {
    let mut found = Vec::new();
    for placeholder_id in hydration.islands.keys() {
        let open = format!("<div id=\"{placeholder_id}\" data-albedo-tier=\"c\"");
        let mut from = 0;
        while let Some(offset) = html[from..].find(&open) {
            let start = from + offset;
            let rest = &html[start + open.len()..];
            let (carried, tail) = if let Some(value) = rest.strip_prefix(PROPS_ATTR) {
                match value.find('"') {
                    Some(close) => {
                        let parsed = serde_json::from_str(&unescape_attr(&value[..close])).ok();
                        (Some(parsed), &value[close + 1..])
                    }
                    None => break,
                }
            } else {
                (None, rest)
            };
            if !tail.starts_with("></div>") {
                from = start + open.len();
                continue;
            }
            let end = html.len() - tail.len() + "></div>".len();
            found.push(Occurrence {
                start,
                end,
                placeholder_id: placeholder_id.clone(),
                carried,
            });
            from = end;
        }
    }
    found
}

fn plain_placeholder(placeholder_id: &str) -> String {
    format!("<div id=\"{placeholder_id}\" data-albedo-tier=\"c\"></div>")
}

/// Remove any `data-albedo-props` left on a Tier-C placeholder this pass did
/// not own — an island with no hydration block. Its props were bound for an
/// island that is not being served; they are not page content.
fn strip_carried_props(html: String) -> String {
    const ON_ISLAND: &str = " data-albedo-tier=\"c\" data-albedo-props=\"";
    if !html.contains(ON_ISLAND) {
        return html;
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html.as_str();
    while let Some(at) = rest.find(ON_ISLAND) {
        let keep = at + " data-albedo-tier=\"c\"".len();
        out.push_str(&rest[..keep]);
        let value = &rest[at + ON_ISLAND.len()..];
        rest = match value.find('"') {
            Some(close) => &value[close + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Inverse of the attribute escape both renderers use (`&`, `<`, `>`, `"`).
fn unescape_attr(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::tier_b::RenderError;
    use async_trait::async_trait;
    use serde_json::json;

    /// Renders an island as its props, so a test can see which props won.
    struct EchoRegistry;

    #[async_trait]
    impl TierBRenderRegistry for EchoRegistry {
        async fn call(
            &self,
            render_fn: &str,
            props: &Value,
            _data: &HashMap<String, Value>,
        ) -> Result<String, RenderError> {
            Ok(format!("<p>{render_fn} {props}</p>"))
        }
    }

    fn hydrated(boot: Value, keys: &[&str]) -> RouteHydration {
        let mut islands = HashMap::new();
        islands.insert(
            "__c_picker_2".to_string(),
            RequestIsland {
                boot_props: boot,
                dynamic_prop_keys: keys.iter().map(ToString::to_string).collect(),
                render: IslandRender::Hydrate {
                    plan_key: "island:Picker".to_string(),
                    component_id: 2,
                },
            },
        );
        RouteHydration {
            islands,
            ..RouteHydration::default()
        }
    }

    fn ctx(id: &str) -> RequestContext {
        RequestContext {
            params: [("id".to_string(), id.to_string())].into_iter().collect(),
            ..RequestContext::default()
        }
    }

    async fn pass(html: &str, hydration: &RouteHydration, id: &str) -> String {
        render_request_islands(
            html.to_string(),
            Some(hydration),
            &ctx(id),
            &IslandServices::default(),
            &EchoRegistry,
        )
        .await
        .0
    }

    #[tokio::test]
    async fn an_island_renders_from_the_props_its_parent_passed_on_this_request() {
        let hydration = hydrated(json!({ "id": null }), &[]);
        let html = r#"<main><div id="__c_picker_2" data-albedo-tier="c" data-albedo-props="{&quot;id&quot;:&quot;7&quot;}"></div></main>"#;
        let out = pass(html, &hydration, "7").await;
        assert!(out.contains(r#"island:Picker {"id":"7"}"#), "{out}");
        // Carried on the island root for the client, escaped.
        assert!(
            out.contains(r#"data-albedo-island="2" data-albedo-props="{&quot;id&quot;:&quot;7&quot;}""#),
            "{out}"
        );
        assert!(!out.contains("data-albedo-tier=\"c\""), "the placeholder is replaced: {out}");
    }

    #[tokio::test]
    async fn an_island_reading_params_itself_gets_this_requests_params() {
        let hydration = hydrated(Value::Null, &["params"]);
        let html = r#"<div id="__c_picker_2" data-albedo-tier="c"></div>"#;
        let seven = pass(html, &hydration, "7").await;
        let zz = pass(html, &hydration, "zz").await;
        assert!(seven.contains(r#"{"params":{"id":"7"}}"#), "{seven}");
        assert!(zz.contains(r#"{"params":{"id":"zz"}}"#), "{zz}");
    }

    #[tokio::test]
    async fn props_equal_to_the_boot_render_keep_it_and_never_ship_the_attribute() {
        let hydration = hydrated(json!({ "label": "x" }), &[]);
        let html = r#"<div id="__c_picker_2" data-albedo-tier="c" data-albedo-props="{&quot;label&quot;:&quot;x&quot;}"></div>"#;
        let out = pass(html, &hydration, "7").await;
        assert_eq!(out, r#"<div id="__c_picker_2" data-albedo-tier="c"></div>"#);
    }

    #[tokio::test]
    async fn a_plain_placeholder_with_no_request_props_is_left_for_the_boot_fill() {
        let hydration = hydrated(json!({}), &[]);
        let html = r#"<div id="__c_picker_2" data-albedo-tier="c"></div>"#;
        assert_eq!(pass(html, &hydration, "7").await, html);
    }

    #[tokio::test]
    async fn props_on_an_island_this_route_does_not_serve_are_stripped() {
        let hydration = hydrated(json!({}), &[]);
        let html = r#"<div id="__c_other_9" data-albedo-tier="c" data-albedo-props="{&quot;secret&quot;:1}"></div>"#;
        let out = pass(html, &hydration, "7").await;
        assert_eq!(out, r#"<div id="__c_other_9" data-albedo-tier="c"></div>"#);
    }

    #[test]
    fn closing_scripts_swap_only_the_overridden_install() {
        let block = RouteHydration {
            closing_scripts: "<driver>".to_string(),
            installs: vec![
                ("__c_a_1".to_string(), "<boot a>".to_string()),
                ("__c_b_2".to_string(), "<boot b>".to_string()),
            ],
            ..RouteHydration::default()
        };
        let overrides: HashMap<String, String> =
            [("__c_b_2".to_string(), "<request b>".to_string())].into_iter().collect();
        assert_eq!(
            block.closing_scripts_for(&overrides),
            "<driver><boot a><request b>"
        );
    }
}
