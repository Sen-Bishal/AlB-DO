use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Tier {
    A,
    B,
    C,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HydrationMode {
    Immediate,
    LazyViewport,
    LazyInteraction,
    LazyIdle,
    None,
    OnVisible,
    OnIdle,
    OnInteraction,
}

impl HydrationMode {
    pub fn into_streaming(self) -> Self {
        match self {
            Self::Immediate => Self::Immediate,
            Self::LazyViewport | Self::OnVisible => Self::LazyViewport,
            Self::LazyInteraction | Self::OnInteraction => Self::LazyInteraction,
            Self::LazyIdle | Self::OnIdle => Self::LazyIdle,
            Self::None => Self::None,
        }
    }
}

/// Describes which components are assigned to a given WebTransport stream slot.
///
/// Emitted into [`RenderManifestV2::wt_streams`] at build time so the dev CLI,
/// `albedo trace`, and the WT client bootstrap can all agree on the slot-to-component
/// mapping without re-running tier analysis at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WTStreamSlot {
    /// Stream slot index (0 = control, 1 = shell, 2 = patches, 3 = prefetch).
    pub slot: u8,
    /// Human-readable label matching `WTRenderMode::as_str()`.
    pub label: String,
    /// Component IDs that stream on this slot.
    pub component_ids: Vec<u64>,
}

/// The full manifest written to disk at build time and loaded at server startup.
///
/// `schema_version` + legacy component fields are retained for backward compatibility
/// with existing tooling while the new route schedule is rolled out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderManifestV2 {
    pub version: u32,
    pub build_id: String,
    pub routes: HashMap<String, RouteManifest>,
    pub assets: AssetManifest,
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub components: Vec<ComponentManifestEntry>,
    #[serde(default)]
    pub parallel_batches: Vec<Vec<u64>>,
    #[serde(default)]
    pub critical_path: Vec<u64>,
    #[serde(default)]
    pub vendor_chunks: Vec<VendorChunk>,
    /// WebTransport stream slot assignments, populated at build time.
    ///
    /// Slot indices follow the `WT_STREAM_SLOT_*` constants in `runtime/webtransport.rs`:
    /// slot 0 = control, 1 = shell, 2 = patches, 3 = prefetch.
    /// Empty when the build predates WT support or when no Tier B/C components exist.
    #[serde(default)]
    pub wt_streams: Vec<WTStreamSlot>,
}

impl RenderManifestV2 {
    pub const SCHEMA_VERSION: &'static str = "2.0";
    pub const VERSION: u32 = 2;

    pub fn legacy_defaults() -> Self {
        Self {
            version: Self::VERSION,
            build_id: String::new(),
            routes: HashMap::new(),
            assets: AssetManifest::default(),
            schema_version: Self::SCHEMA_VERSION.to_string(),
            generated_at: String::new(),
            components: Vec::new(),
            parallel_batches: Vec::new(),
            critical_path: Vec::new(),
            vendor_chunks: Vec::new(),
            wt_streams: Vec::new(),
        }
    }
}

/// Per-route streaming schedule produced at compile time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteManifest {
    pub route: String,
    pub shell: HtmlShell,
    pub tier_a_root: Vec<RenderedNode>,
    pub tier_b: Vec<TierBNode>,
    pub tier_c: Vec<TierCNode>,
    /// Phase P · broadcast topics referenced by any component on this
    /// route. The streaming handler auto-subscribes each WT session
    /// to these at render time so JSX-side `useSharedSlot("topic")`
    /// resolves without explicit subscribe.
    #[serde(default)]
    pub shared_slot_topics: Vec<String>,
    /// PRISM · the partitioned shared-slot bindings **this route's own
    /// components** read, still unresolved — a topic identity needs the
    /// request's params, which do not exist at build time.
    ///
    /// Per-route, unlike `shared_slot_topics` above, and that difference is
    /// load-bearing rather than an optimization. A static topic is a
    /// compile-time global, so listing every project topic on every route only
    /// costs a few extra `mpsc::Sender` clones — nothing is grantable that is
    /// not already public. A *partition* resolves against **this** request's
    /// params, so a project-wide list would take another route's binding
    /// (`comments.where({ doc: params.id })`) and resolve it against this
    /// route's `id`, handing the lane a partition of a collection this page
    /// never renders. That is precisely the read capability PRISM invariant 2
    /// exists to deny: a topic is reachable only through a route that renders
    /// it.
    #[serde(default)]
    pub shared_slot_partitions: Vec<PartitionTopicSpec>,
    /// APERTURE · the declared external resources this route reads.
    ///
    /// Per-route for exactly the reason `shared_slot_partitions` is: a source
    /// binding whose arguments come from `params` resolves against **this**
    /// request's params, so a project-wide list would resolve another route's
    /// binding against this route's values. Invariant 2 is the same for both
    /// derivations — a topic is reachable only through a route that renders it.
    #[serde(default)]
    pub shared_slot_sources: Vec<SourceTopicSpec>,
    /// Phase P · TS-side action handler names + their wire
    /// `action_id`s for this route. Populated once Stream C lands
    /// the `action()` extractor; the field exists now so manifests
    /// produced by intermediate builds round-trip cleanly.
    #[serde(default)]
    pub action_ids: Vec<RouteActionEntry>,
    /// Phase P · ordered layout chain (outermost → leaf) for this
    /// route. Each entry is a component name resolved through
    /// `discover_routes::DiscoveredRoute.layout_chain`. Render-side
    /// composition (Stream E.1) wraps the route's HTML in each
    /// layout's HTML in order.
    #[serde(default)]
    pub layout_chain: Vec<String>,
    /// Phase P · component name of the `error.tsx` boundary for this
    /// route, if any. Streaming handler serves this when a Tier-C
    /// resolution fails. Stream E.2 populates this; field added now
    /// so the schema is stable.
    #[serde(default)]
    pub error_component: Option<String>,
    /// Phase P · component name of the `loading.tsx` placeholder for
    /// this route, if any. Streaming handler serves this while
    /// Tier-C resolves. Stream E.2 populates this.
    #[serde(default)]
    pub loading_component: Option<String>,
    /// Gate 2 · B — resolved document metadata for this route's
    /// `<head>`. Composed (last-writer-wins) from three layered
    /// sources: the static `export const metadata` object, a dynamic
    /// `generateMetadata()` evaluated per request, and JSX-hoisted
    /// `<title>`/`<meta>` tags. `Default` (all empty) preserves the
    /// historical shell `<head>` exactly — the `ALBEDO {route}` title
    /// fallback still applies — so routes that author no metadata are
    /// byte-identical to pre-B builds.
    #[serde(default)]
    pub metadata: RouteMetadata,
    /// Gate 2 · B slice 3 — when the route's leaf component module exports a
    /// `generateMetadata(props)` function, this carries the boot-plan key
    /// (the leaf component name) the serve path invokes per request to resolve
    /// dynamic `<head>` metadata. `None` (the common case) means the route's
    /// head is fully static and the pre-baked shell stands unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_metadata: Option<String>,
}

/// Gate 2 · B — the resolved per-route document metadata destined for
/// the shell `<head>`. Authoring-surface agnostic: the builder lowers
/// `export const metadata` / `generateMetadata()` / JSX head tags into
/// this one shape, then the shell renders it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Resolved `<meta>` tags in author order (both `name=` and
    /// `property=` flavours; `description` is NOT duplicated here — it
    /// rides the `description` field and the shell emits its tag).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meta: Vec<MetaTag>,
}

impl RouteMetadata {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && self.meta.is_empty()
    }

    /// Layer `other` on top of `self` (last-writer-wins): any scalar
    /// `other` sets overrides; meta tags append. This is how the static
    /// base composes with the dynamic and JSX-hoisted overrides.
    pub fn merge(&mut self, other: RouteMetadata) {
        if other.title.is_some() {
            self.title = other.title;
        }
        if other.description.is_some() {
            self.description = other.description;
        }
        self.meta.extend(other.meta);
    }
}

/// One resolved `<meta>` tag. `attr` is the key-carrying attribute —
/// `"name"` for standard + twitter tags, `"property"` for Open Graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetaTag {
    pub attr: String,
    pub key: String,
    pub content: String,
}

/// PRISM · one `useSharedSlot(<collection>.where({ <column>: <key source> }))`
/// binding, carried to serve time unresolved. The key source is a route param
/// (`params.id`) or the signed-in principal (`user.id`) — see
/// [`PartitionKeySource`].
///
/// The extractor lowers the TSX to this; [`crate::runtime::resolve_partition_topics`]
/// turns it into a topic identity once a request supplies the params. Nothing
/// here is a topic *string* — that is the whole point of PRISM § 3.2: the author
/// never spells one, so two logically distinct partitions aliasing onto one
/// channel is unexpressible rather than merely checked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PartitionTopicSpec {
    /// The component-local name the binding is assigned to
    /// (`const rows = useSharedSlot(...)` → `"rows"`). This is the key the
    /// transpiled `__albedo_topic("rows")` looks up in `host.topics`, so it is
    /// how a resolved topic reaches the component that reads it.
    pub binding: String,
    /// The declared collection name — the `forge` block key.
    pub collection: String,
    /// The column `.where({ … })` named. Already checked against the
    /// collection's declared `partition_by` at build time
    /// (`validate_partition_bindings`); kept because the write path resolves by
    /// column and a mismatch here would be silent.
    pub column: String,
    /// Where the partition key comes from: `params.id` → `RouteParam("id")`,
    /// `user.id` → `Identity`.
    pub key: PartitionKeySource,
}

/// Where a [`PartitionTopicSpec`]'s key is read from on a request.
///
/// The serve-time mirror of [`crate::transforms::shared_slots::KeySource`],
/// which is the compile-time one. They are separate types on purpose: this one
/// crosses the manifest and so is a wire format, and collapsing them would let a
/// change to the extractor's vocabulary silently change a serialized artifact.
///
/// Serialized externally tagged, so a manifest diff reads as
/// `"key": {"route_param": "id"}` or `"key": "identity"` — the distinction a
/// reviewer most needs to see is the one between "keyed by the URL" and "keyed
/// by who is asking".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PartitionKeySource {
    /// A route parameter: `params.id` on `/room/[id]`.
    RouteParam(String),
    /// The authenticated principal's id — AUTH § 3, item 5 P1.
    ///
    /// 🔒 **Resolves to no topic at all when the request is anonymous.** Not to
    /// an empty key, and not to a shared fallback: a partition every signed-out
    /// visitor could name would be one namespace holding everyone's rows, which
    /// is the exact failure the derived-authorization design exists to make
    /// unexpressible. See [`crate::runtime::resolve_partition_topics`].
    Identity,
}

impl PartitionKeySource {
    /// The route param name, when the key comes from the URL.
    ///
    /// `None` for [`Self::Identity`] — which is the honest answer, and callers
    /// that reconstruct a `params` object for a re-render depend on it being
    /// absent rather than empty.
    #[must_use]
    pub fn route_param(&self) -> Option<&str> {
        match self {
            Self::RouteParam(name) => Some(name.as_str()),
            Self::Identity => None,
        }
    }
}

/// APERTURE · one `useSharedSlot(<source>.<route>({ … }))` binding on a route.
///
/// The sibling of [`PartitionTopicSpec`], and shaped the same way on purpose:
/// the extractor lowers TSX to this, and
/// [`crate::runtime::resolve_source_topics`] turns it into a topic identity once
/// a request supplies the params. Nothing here is a topic *string* either.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceTopicSpec {
    /// The component-local name the binding is assigned to — the key the
    /// transpiled `__albedo_topic("repo")` looks up in `host.topics`.
    pub binding: String,
    /// The declared source name — the `sources` block key.
    pub source: String,
    /// The route name called on it.
    pub route: String,
    /// Arguments by name, sorted. Checked against the declared route's path
    /// placeholders at build time (`validate_source_bindings`).
    pub args: Vec<SourceArgSpec>,
}

/// Where one source-route argument's value comes from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum SourceArgSpec {
    /// Bound from a route parameter: `owner: params.owner`.
    Param {
        /// The argument name, matching a `{placeholder}` in the route's path.
        name: String,
        /// The route parameter supplying it.
        param: String,
    },
    /// Fixed at build time: `owner: "anthropics"`.
    Literal {
        /// The argument name, matching a `{placeholder}` in the route's path.
        name: String,
        /// The value.
        value: String,
    },
}

impl SourceArgSpec {
    /// The argument name, whichever variant this is.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            SourceArgSpec::Param { name, .. } | SourceArgSpec::Literal { name, .. } => name,
        }
    }
}

/// Phase P · one TS-authored action handler discovered on a route.
/// `action_id` is `FNV-1a-32(name)` — the same hash the form
/// extractor's `allocate_form_action_id` produces, so the wire
/// envelope's `action_id` looks the route up directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteActionEntry {
    pub name: String,
    pub action_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderedNode {
    pub component_id: String,
    pub placeholder_id: String,
    pub html: String,
    pub position: DomPosition,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TierBNode {
    pub component_id: String,
    pub placeholder_id: String,
    pub render_fn: String,
    pub static_props: Value,
    pub dynamic_prop_keys: Vec<String>,
    pub data_deps: Vec<DataDep>,
    pub tier_a_children: Vec<RenderedNode>,
    pub position: DomPosition,
    pub timeout_ms: u64,
    pub fallback_html: Option<String>,
    /// Phase P · pre-rendered initial HTML for this Tier-B component,
    /// produced at build time by `render_entry_with_broadcast` against
    /// a fresh empty slot store. The streaming handler inlines this
    /// into the shell instead of the placeholder fallback. `None`
    /// when the build pipeline couldn't render (missing source,
    /// transient error) — falls back to `fallback_html`.
    #[serde(default)]
    pub initial_html: Option<String>,
    /// Phase P · bincode-encoded `OpcodeFrame` carrying the initial
    /// hydration payload (`BindEvent` + `SetTextRef` + initial
    /// `SlotSet`). The streaming handler ships these bytes verbatim
    /// on the WT patches lane so bakabox wires up the island on
    /// first paint. Empty when no Phase K hooks / events fired.
    /// Encoding via `crate::ir::wire::encode_frame` matches the
    /// runtime wire format — no schema drift possible.
    #[serde(default)]
    pub initial_opcode_frame: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TierCNode {
    pub component_id: String,
    pub placeholder_id: String,
    pub bundle_path: String,
    pub initial_props: Value,
    pub hydration_mode: HydrationMode,
    pub position: DomPosition,
    /// True when this component carries an effect hook (`useEffect` /
    /// `useLayoutEffect` / `useInsertionEffect`) or another mount-time side
    /// effect. Sourced from the tiering analysis' `EffectProfile::side_effects`,
    /// so the serve path never has to re-parse source to learn it. Consumed by
    /// the reactive serve-wire builder: a side-effecting island is excluded from
    /// fine-grained binding mode (whose descriptor has no notion of effects) and
    /// falls back to full A3 hydration, where its effect actually runs.
    #[serde(default)]
    pub side_effects: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DomPosition {
    pub parent_placeholder: Option<String>,
    pub slot: String,
    pub order: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataDep {
    pub key: String,
    pub source: DataSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataSource {
    DbQuery {
        query: String,
        param_keys: Vec<String>,
    },
    HttpFetch {
        url_template: String,
        method: String,
    },
    Cache {
        cache_key_template: String,
        ttl_s: u64,
    },
    RequestContext {
        key: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HtmlShell {
    pub doctype_and_head: String,
    pub body_open: String,
    pub body_close: String,
    pub shim_script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetManifest {
    pub chunks: HashMap<String, String>,
    pub css: Vec<String>,
    pub runtime: String,
}

impl Default for AssetManifest {
    fn default() -> Self {
        Self {
            chunks: HashMap::new(),
            css: Vec::new(),
            runtime: "/_albedo/runtime.js".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComponentManifestEntry {
    pub id: u64,
    pub name: String,
    pub module_path: String,
    pub tier: Tier,
    pub weight_bytes: u64,
    pub priority: f64,
    pub dependencies: Vec<u64>,
    pub can_defer: bool,
    pub hydration_mode: HydrationMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VendorChunk {
    pub chunk_name: String,
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StaticSliceArtifactEntry {
    pub component_id: u64,
    pub module_path: String,
    pub source_hash: u64,
    pub eligible: bool,
    pub ineligibility_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StaticSliceArtifactManifest {
    pub version: String,
    pub manifest_schema_version: String,
    pub manifest_generated_at: String,
    pub entry_component_id: Option<u64>,
    pub slices: Vec<StaticSliceArtifactEntry>,
}

impl StaticSliceArtifactManifest {
    pub const VERSION: &'static str = "1.0";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrecompiledRuntimeModuleEntry {
    pub component_id: u64,
    pub module_path: String,
    pub source_hash: u64,
    pub compiled_script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrecompiledRuntimeModuleSkip {
    pub component_id: u64,
    pub module_path: String,
    pub source_hash: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrecompiledRuntimeModulesArtifact {
    pub version: String,
    pub engine: String,
    pub manifest_schema_version: String,
    pub manifest_generated_at: String,
    pub modules: Vec<PrecompiledRuntimeModuleEntry>,
    pub skipped: Vec<PrecompiledRuntimeModuleSkip>,
}

impl PrecompiledRuntimeModulesArtifact {
    pub const VERSION: &'static str = "1.0";
    pub const ENGINE_QUICKJS: &'static str = "quickjs";
}

#[cfg(test)]
mod tests {
    use super::{HydrationMode, PartitionKeySource, PartitionTopicSpec};

    #[test]
    fn test_hydration_mode_none_stays_none_for_streaming() {
        assert_eq!(HydrationMode::None.into_streaming(), HydrationMode::None);
    }

    /// AUTH item 5 P1 · the manifest is an artifact a reviewer reads, so the
    /// spelling is pinned rather than left to serde's defaults. The distinction
    /// this encodes — *keyed by the URL* vs *keyed by who is asking* — is the one
    /// an auditor most needs to see at a glance in a diff.
    #[test]
    fn a_partition_key_source_serializes_to_a_readable_shape() {
        let by_param = serde_json::to_string(&PartitionKeySource::RouteParam("id".to_string()))
            .expect("serialize");
        assert_eq!(by_param, r#"{"route_param":"id"}"#);

        let by_identity = serde_json::to_string(&PartitionKeySource::Identity).expect("serialize");
        assert_eq!(by_identity, r#""identity""#);
    }

    #[test]
    fn a_partition_spec_round_trips_through_the_manifest() {
        for key in [
            PartitionKeySource::RouteParam("id".to_string()),
            PartitionKeySource::Identity,
        ] {
            let spec = PartitionTopicSpec {
                binding: "rows".to_string(),
                collection: "todos".to_string(),
                column: "owner".to_string(),
                key,
            };
            let encoded = serde_json::to_string(&spec).expect("serialize");
            let decoded: PartitionTopicSpec = serde_json::from_str(&encoded).expect("deserialize");
            assert_eq!(spec, decoded);
        }
    }

    /// `route_param()` returning `None` for `Identity` is depended on by the row
    /// projector, which reconstructs props for a re-render: an identity key must
    /// land under `user`, never under a param name it does not have.
    #[test]
    fn identity_reports_no_route_param() {
        assert_eq!(
            PartitionKeySource::RouteParam("id".to_string()).route_param(),
            Some("id")
        );
        assert_eq!(PartitionKeySource::Identity.route_param(), None);
    }
}
