use crate::actions::{ActionHandler, SessionSlots};
use crate::api::ApiHandler;
use crate::config::AppConfig;
use crate::contract::{
    AllowAllAuthProvider, AuthDecision, AuthProvider, LayoutHandler, PropsLoader, RouteHandler,
    RuntimeMiddleware,
};
use crate::error::RuntimeError;
use crate::handlers::action::{run_action_request, ActionRegistry, FormActionIds, GatedActionIds};
use crate::handlers::api::dispatch_api_route;
use crate::handlers::public_assets::PublicAssets;
use crate::handlers::{
    streaming_handler, streaming_handler_with_match, StreamingAppState, StreamingTransportConfig,
};
use crate::inspector::{
    self as inspector_routes, GraphSnapshot as InspectorGraphSnapshot, InspectorState,
};
use crate::lifecycle::{RequestContext, ResponseBody, ResponsePayload};
use crate::render::csrf::CsrfRegistry;
use crate::render::tier_b::{PooledTierBRenderRegistry, SharedRenderServices, TierBOpcodeRegistry};
use crate::renderer_runtime::RendererRuntime;
use crate::routing::{CompiledRouter, HttpMethod, RouteMatch, RouteTarget};
use crate::webtransport::{WebTransportRuntime, WebTransportSessionRegistry};
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use dom_render_compiler::runtime::pipeline::FourLaneRuntimePipeline;
use dom_render_compiler::runtime::{
    resolve_partition_topics, BroadcastRegistry, ResolvedPartition, ResolvedSourceTopic, SessionId,
    SlotStore,
};
use dom_render_compiler::shutter::{Cost, Key, OperationClass, Verdict};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::compression::predicate::{And, DefaultPredicate, NotForContentType, Predicate};
use tower_http::compression::CompressionLayer;
use tracing::{debug, error, info, warn};

const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Bridge from a Phase-K `CompiledProject`'s handler registry to the
/// server's `ActionHandler` trait. One adapter is registered per
/// `(proxy_id, handler)` pair by `register_compiled_project`.
///
/// `handle` constructs a `SessionSlotView` from the dispatcher's
/// `SessionSlots` (same `Arc<SlotStore>`, same `SessionId`) and calls
/// the project's `invoke_action`. The drain happens inside
/// `invoke_action` so the explicit return already carries the
/// `SlotSet` opcodes; the dispatcher's follow-up drain is then a
/// no-op, which is idempotent and safe.
/// Runtime singletons that MUST survive a dev world-swap.
///
/// A hot reload rebuilds the whole render world from disk and swaps it in. Two
/// things must NOT be rebuilt with it: the broadcast registry — because it
/// holds live topic **values** (hydrated once at boot from the substrate) and
/// live **subscribers** (open SSE/WT connections) that a fresh empty registry
/// would strand — and the FORGE substrate handle, opened once in `run()`.
/// Bundling them here lets a reload thread the SAME instances into the fresh
/// build ([`AlbedoServerBuilder::with_live_runtime`]), so the swap replaces
/// *build output* (world, Tier-B plan, engine pool, row projector) while
/// *live state* carries across untouched.
///
/// The row projector is build output, not live state — it closes over the new
/// plan and pool, so a reload must replace it. It lives here anyway, as a
/// swappable slot the build re-fills, because the two readers that need the
/// *current* projector (the action adapters' write path and the dispatcher's
/// reconnect-resync path) both outlive any single world and so must reach it
/// through one stable handle rather than a per-build clone.
#[derive(Clone)]
pub(crate) struct LiveRuntime {
    broadcast: Arc<BroadcastRegistry>,
    forge_substrate: Arc<std::sync::OnceLock<Arc<dyn dom_render_compiler::forge::DataSubstrate>>>,
    row_projector:
        Arc<std::sync::RwLock<Option<Arc<dyn dom_render_compiler::forge::RowProjector>>>>,
    /// The FORGE collection registry — the `topic → (query, schema)` allowlist
    /// the write path resolves against and boot hydrates from. App-static and
    /// immutable, so it is built once and shared (a hot reload reuses it rather
    /// than rebuilding). Phase 1: the built-in guestbook default; Phase 2: the
    /// app-declared schema, loaded here at construction.
    forge_schema: Arc<dom_render_compiler::forge::ForgeSchema>,
    /// APERTURE · the declared-source read path, or `None` when the app declared
    /// no `sources` block.
    ///
    /// Held here beside `forge_schema` because it is the same kind of thing: an
    /// app-static registry plus the client that derives from it. Pinned in the
    /// persistent tier rather than the world so a dev hot reload keeps its
    /// response cache — re-minting it on every file save would make every source
    /// cold on every keystroke and hammer the upstream while an author types.
    source_reader: Arc<std::sync::OnceLock<Arc<dom_render_compiler::aperture::SourceReader>>>,
    /// APERTURE A2 · the outbound client an action body's `fetch()` goes
    /// through. Always present after `build()`.
    ///
    /// Separate from `source_reader` because it must exist for an app that
    /// declared no `sources` at all — a bare `fetch()` is § 6's escape hatch and
    /// does not require a declaration — but it is the **same client** when there
    /// is a reader, so a declared host's connection pool, its egress allowlist
    /// and a bare call's are one thing rather than two that can disagree.
    ///
    /// Live state, not build output: it owns a connection pool, so re-minting it
    /// on every dev file save would drop every keep-alive an author is watching.
    aperture_client: Arc<std::sync::OnceLock<Arc<dom_render_compiler::aperture::ApertureClient>>>,
    /// AUTH · the request-time identity path.
    ///
    /// Held here, beside the FORGE substrate and the APERTURE reader, because
    /// all three answer one question — *what does this request get to see* — and
    /// keeping them on one handle is what stops a request from resolving its
    /// identity through a different world than its data. `None` until boot
    /// installs one; an app that declared no providers installs one anyway, and
    /// it resolves everybody as anonymous without spending a query.
    auth: Arc<std::sync::OnceLock<Arc<crate::auth::AuthRuntime>>>,
}

impl LiveRuntime {
    /// Fresh, empty singletons for a first boot. `run()` fills the substrate;
    /// `build()` installs the projector. The schema is app-static, so it is
    /// resolved here, once, and never swapped by a reload.
    fn new() -> Self {
        Self {
            broadcast: Arc::new(BroadcastRegistry::new()),
            forge_substrate: Arc::new(std::sync::OnceLock::new()),
            row_projector: Arc::new(std::sync::RwLock::new(None)),
            forge_schema: Arc::new(dom_render_compiler::forge::ForgeSchema::guestbook_default()),
            source_reader: Arc::new(std::sync::OnceLock::new()),
            aperture_client: Arc::new(std::sync::OnceLock::new()),
            auth: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Install the AUTH request path. Idempotent, on the same terms as the
    /// APERTURE reader: a dev world swap must not replace the runtime that open
    /// sessions were resolved through.
    fn install_auth(&self, auth: Arc<crate::auth::AuthRuntime>) {
        let _ = self.auth.set(auth);
    }

    /// Resolve this request's identity.
    ///
    /// The single entry point for all three paths — render, action dispatch and
    /// the PHOSPHOR subscribe lane. One function rather than three call sites so
    /// there is exactly one answer per request to *who is this*, and no way for
    /// the page and its live lane to disagree about it.
    ///
    /// Before boot installs a runtime, everybody is anonymous — which is the
    /// same answer an app with no declared providers gets, and the safe one.
    async fn identity(&self, headers: &axum::http::HeaderMap) -> crate::auth::Identity {
        match self.auth.get() {
            Some(auth) => auth.resolve(headers).await,
            None => crate::auth::Identity::Anonymous,
        }
    }

    /// The installed AUTH runtime, for the login flows that need to *write* a
    /// session rather than read one.
    ///
    /// `None` before boot installs one — which the sign-in endpoints treat as
    /// "this app has no auth", the same answer an app that declared no providers
    /// gets. Fail closed by returning nothing rather than by constructing an
    /// empty runtime here: a default-constructed one would carry a substrate
    /// nobody chose.
    fn auth(&self) -> Option<&Arc<crate::auth::AuthRuntime>> {
        self.auth.get()
    }

    /// Install the APERTURE read path. Idempotent; a second call is ignored, so
    /// a dev world swap cannot replace a warm cache with a cold one.
    ///
    /// Also adopts the reader's client as the workflow client, unless one is
    /// already installed. One statement of intent: the `sources` block is what
    /// derives the egress allowlist (invariant 2.7), and a workflow reaching a
    /// declared host through a *different* policy object would be a second place
    /// for that intent to live.
    fn install_source_reader(&self, reader: Arc<dom_render_compiler::aperture::SourceReader>) {
        let _ = self.aperture_client.set(Arc::clone(reader.client()));
        let _ = self.source_reader.set(reader);
    }

    /// Install the outbound client for action-body `fetch()`. Idempotent, on the
    /// same terms as the reader.
    fn install_aperture_client(
        &self,
        client: Arc<dom_render_compiler::aperture::ApertureClient>,
    ) {
        let _ = self.aperture_client.set(client);
    }

    /// The workflow client, if one has been installed.
    fn aperture_client(&self) -> Option<&Arc<dom_render_compiler::aperture::ApertureClient>> {
        self.aperture_client.get()
    }

    /// The installed APERTURE read path, if the app declared any sources.
    fn source_reader(&self) -> Option<&Arc<dom_render_compiler::aperture::SourceReader>> {
        self.source_reader.get()
    }

    /// The current row projector, cloned out (a refcount bump). Cloned rather
    /// than borrowed so the caller can hold it across an `.await` without
    /// keeping the lock — the write path awaits `apply_writes`, the resync path
    /// awaits `project_rows`.
    fn projector(&self) -> Option<Arc<dyn dom_render_compiler::forge::RowProjector>> {
        self.row_projector
            .read()
            .expect("row projector lock poisoned")
            .clone()
    }

    /// Install the projector the current build produced, replacing any prior
    /// one. Called by `build()` on first boot AND on every dev reload.
    fn install_projector(&self, projector: Arc<dyn dom_render_compiler::forge::RowProjector>) {
        *self
            .row_projector
            .write()
            .expect("row projector lock poisoned") = Some(projector);
    }

    /// PRISM · materialise one partition into the registry if it is not already
    /// there, and report whether the topic ended up live.
    ///
    /// The read-through half of "the substrate is the truth, the value is a
    /// cache". Three outcomes, all of them fine for the page:
    ///
    /// - **hit** — the entry exists; stamp it so the byte budget sees a read as
    ///   use, and do no I/O. A hit is authoritative rather than merely likely:
    ///   within a process the write path keeps every warm partition exact by
    ///   splicing or evicting it, so there is no staleness window to re-check.
    /// - **miss** — one indexed range scan over the partition, then mint.
    /// - **refused** — a slot-id collision, a collection that is not declared, a
    ///   substrate error. Logged with the topic named; the topic stays
    ///   unregistered and the route degrades to no live data.
    async fn warm_partition(&self, partition: &ResolvedPartition) -> bool {
        if self.broadcast.touch(&partition.topic) {
            return true;
        }

        let Some(substrate) = self.forge_substrate.get() else {
            return false;
        };
        let Some(collection) = self.forge_schema.slot_for_topic(&partition.collection) else {
            // The boot check (`validate_partition_bindings`) rules this out for
            // any binding the compiler saw, so reaching it means the schema and
            // the manifest were built from different sources — worth a warning
            // rather than a silent empty room.
            warn!(
                target: "albedo.prism",
                topic = %partition.topic,
                collection = %partition.collection,
                "partition names a collection the FORGE schema does not declare; \
                 route will render without live data"
            );
            return false;
        };

        let bytes = match dom_render_compiler::forge::skeleton::materialize_slot(
            substrate.as_ref(),
            collection,
            Some(partition.key.as_str()),
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!(
                    target: "albedo.prism",
                    topic = %partition.topic,
                    error = %err,
                    "partition materialisation failed; route will render without live data"
                );
                return false;
            }
        };

        match self.broadcast.try_topic_partition(
            partition.topic.clone(),
            Arc::from(partition.collection.as_str()),
            Arc::from(partition.key.as_str()),
            bytes,
        ) {
            Ok(_) => true,
            Err(err) => {
                // The § 5.3 guard firing. One room loudly refuses to go live,
                // which is the trade the guard exists to make — the alternative
                // is two rooms silently sharing a wire slot and cross-delivering
                // each other's rows.
                warn!(
                    target: "albedo.prism",
                    topic = %partition.topic,
                    error = %err,
                    "partition refused: wire slot already held by another topic"
                );
                false
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::topics::TopicWarmer for LiveRuntime {
    /// Warm each partition, then hold the process to its byte budget.
    ///
    /// The sweep runs **after** the warm rather than before, so the partitions
    /// this request just asked for are the newest entries and cannot be the ones
    /// reclaimed — warming a room only to evict it before rendering would be a
    /// slow way to serve nothing. Subscribed partitions are never candidates
    /// either, so a room with a live tab on it survives regardless.
    async fn warm(&self, partitions: &[ResolvedPartition]) {
        for partition in partitions {
            self.warm_partition(partition).await;
        }
        if !partitions.is_empty() {
            let dropped = self
                .broadcast
                .enforce_byte_budget(dom_render_compiler::runtime::DEFAULT_TOPIC_VALUE_BUDGET);
            if dropped > 0 {
                debug!(
                    target: "albedo.prism",
                    dropped,
                    "topic value cache over budget; reclaimed idle partitions"
                );
            }
        }
    }

    /// APERTURE · read each declared source and publish its body as the topic's
    /// value.
    ///
    /// Fail-soft per source, not per batch: a dashboard reading six widgets
    /// where one upstream is down must still show the other five. A failed read
    /// leaves its topic unregistered, which renders an empty slot rather than
    /// publishing "this resource is empty" as though it were an answer.
    ///
    /// The refresh window is enforced *inside* the client, so calling this on
    /// every render and every subscribe is cheap by construction — a fresh entry
    /// never reaches the wire.
    ///
    /// It is **not** the schedule, though an earlier version of this comment
    /// claimed it was. A cache is consulted only when something asks, and a
    /// viewer sitting on an open tab asks for nothing; keeping a topic current
    /// for that viewer is [`dom_render_compiler::aperture::RefreshLoop`]'s job,
    /// spawned in `run()`. This path is the call-driven half — first render,
    /// new subscriber — and the two share
    /// [`dom_render_compiler::aperture::refresh_topic`] so they cannot disagree
    /// about when a poll is worth a frame.
    async fn warm_sources(&self, sources: &[ResolvedSourceTopic]) {
        let Some(reader) = self.source_reader.get() else {
            return;
        };
        for wanted in sources {
            let resolved = dom_render_compiler::aperture::ResolvedSource {
                topic: wanted.topic.clone(),
                url: wanted.url.clone(),
                source: wanted.source.clone(),
                route: wanted.route.clone(),
            };
            dom_render_compiler::aperture::refresh_topic(
                reader.as_ref(),
                self.broadcast.as_ref(),
                &resolved,
            )
            .await;
        }
    }
}

// Phase P · Stream C.2 — adapter carries the per-server
// `Arc<BroadcastRegistry>` so `handle()` can install it into the
// interpreter's `PHASE_K_BROADCAST` thread-local for the duration of
// the action dispatch. Without that install, a TS handler calling
// `broadcast(topic, updater)` would surface a clean error from the
// interpreter ("broadcast() unavailable") because the builtin only
// resolves when the thread-local is set.
//
// FORGE + S4 · the adapter reaches the substrate and the row projector
// through the shared `LiveRuntime` so a dev hot-swap can't hand it a stale
// (rebuilt-empty) registry or an unfilled substrate cell.
struct CompiledProjectActionAdapter {
    project: Arc<dom_render_compiler::runtime::CompiledProject>,
    action_id: u32,
    /// A1 · *scaffolding* — when `Some`, `handle()` routes the action through
    /// the QuickJS executor (`invoke_action_quickjs_with_broadcast`) on a pooled
    /// engine instead of the pure-Rust `invoke_action_with_broadcast`. Currently
    /// wired but left `None` by `register_compiled_project` so the default path
    /// is unchanged; flipping it on is remaining slice #1 of the A1 bridge (see
    /// `engine_pool` module docs + `project_a1_bridge`). The QuickJS path unlocks
    /// loops/`try`/array methods in action bodies that the pure-Rust path rejects.
    engine_pool: Option<Arc<crate::engine_pool::QuickJsEnginePool>>,
    /// The broadcast registry, FORGE substrate handle, and row projector — all
    /// reached through the shared [`LiveRuntime`] so a dev hot-swap hands the
    /// adapter the live instances, never a rebuilt-empty registry or an
    /// unfilled substrate cell. See [`LiveRuntime`].
    live: LiveRuntime,
    /// APERTURE A2 · the build this adapter belongs to (§ 11 R8).
    ///
    /// Build output, so it lives here and not on [`LiveRuntime`] — a dev reload
    /// mints new adapters and this value goes with them, which is exactly what
    /// makes it a *different* build.
    build_id: Arc<str>,
}

#[async_trait::async_trait]
impl ActionHandler for CompiledProjectActionAdapter {
    async fn handle(
        &self,
        // AUTH F1 · read at last. The context has always reached this method and
        // was always ignored; it is what carries the request's principal to the
        // write path, which is the one place a row's owner can be checked.
        ctx: &RequestContext,
        envelope: &dom_render_compiler::ir::action::ActionEnvelope,
        slots: SessionSlots,
    ) -> Result<Vec<dom_render_compiler::ir::opcode::Instruction>, RuntimeError> {
        debug_assert_eq!(
            envelope.action_id, self.action_id,
            "compiled adapter mis-dispatched: registered for {}, got envelope for {}",
            self.action_id, envelope.action_id,
        );
        let view = dom_render_compiler::runtime::SessionSlotView::new(
            slots.session_id(),
            slots.store().clone(),
        );

        // A1 · QuickJS path. When a pool is wired, ship the action to a pooled
        // engine on its dedicated thread: the closure gets `&mut QuickJsEngine`,
        // runs the broadcast-aware executor, and its result crosses back over
        // the thread boundary. Everything captured is `Send` (Arc clones + an
        // owned envelope clone).
        //
        // APERTURE A2 · and it runs **one pass**, not one dispatch. A body that
        // called out comes back suspended with its requests staged; the engine
        // is checked back in, the round trip happens with nothing checked out,
        // and the body runs again against a journal that answers it. That is
        // invariant 2.6 — the seam where the engine is either released across
        // the RTT or quietly held — and gate 5 measured the difference at
        // 403.9 ms against 52.7 ms, peak 2 engines in flight against 16.
        //
        // The loop belongs here because resolving is async and dispatch is not.
        // `drive_workflow` owns the caps, the ordering and the commit rule; this
        // closure owns only "run one pass on an engine".
        if let Some(pool) = &self.engine_pool {
            let action_id = self.action_id;
            let Some(client) = self.live.aperture_client() else {
                return Err(RuntimeError::RequestHandling(format!(
                    "action {action_id} cannot run: no APERTURE client is installed, so an \
                     outbound fetch() has nothing to send it. This is a server construction \
                     bug — `build()` installs one."
                )));
            };

            // A3 · the workflow's identity, and whether it can be resumed.
            //
            // 🔑 **A random id per dispatch makes a persisted log unreachable.**
            // That is why A3 is not "write the journal down": a uuid minted here
            // is gone with the process, so a retry after a crash mints a new one,
            // finds nothing, and re-issues every completed call under fresh keys.
            // Persistence without stable identity buys a log nobody can ever
            // find again.
            let identity = workflow_identity(self.action_id, ctx.principal.as_ref(), envelope);

            let substrate = self.live.forge_substrate.get().cloned();
            let ledger = substrate
                .as_ref()
                .map(|s| dom_render_compiler::aperture::JournalLedger::new(s.as_ref()));

            // A3 · the submit that already happened.
            //
            // 🔑 Resuming the *steps* stops a completed upstream call from being
            // re-issued. It does not stop the body's own effects — a FORGE
            // append — from being applied a second time, because a replayed body
            // runs its writes again. So a workflow that finished answers from
            // the log instead of running at all, which is what turns "no
            // duplicate charge" into "no duplicate submit".
            //
            // Only for a resumable identity: an id this process invented cannot
            // have finished in an earlier one.
            if identity.resumable {
                if let Some(ledger) = &ledger {
                    if let Ok(Some(bytes)) =
                        ledger.completed(&identity.id, self.build_id.as_ref()).await
                    {
                        use dom_render_compiler::ir::wire::WireDecode;
                        if let Ok((instructions, _)) =
                            Vec::<dom_render_compiler::ir::opcode::Instruction>::wire_decode(&bytes)
                        {
                            tracing::debug!(
                                target: "albedo.aperture.ledger",
                                workflow = %identity.id,
                                "answering a repeated submit from the recorded result"
                            );
                            return Ok(instructions);
                        }
                    }
                }
            }

            // Resume: a retry of the same intention picks the log back up rather
            // than starting a second one. A log that will not load is not fatal —
            // starting over is safe precisely because every step it replays
            // carries its own idempotency key, so the upstream deduplicates what
            // this server cannot remember.
            let resumed = match (&ledger, identity.resumable) {
                (Some(ledger), true) => ledger.load(&identity.id).await.ok().flatten(),
                _ => None,
            };
            let mut journal = resumed.unwrap_or_else(|| {
                dom_render_compiler::aperture::Journal::new(
                    identity.id.clone(),
                    self.build_id.as_ref(),
                )
            });

            let durability = ledger.as_ref().map(|ledger| {
                dom_render_compiler::aperture::Durability {
                    ledger,
                    workflow_id: &identity.id,
                    build_id: self.build_id.as_ref(),
                }
            });

            let (instructions, writes) = dom_render_compiler::aperture::drive_workflow(
                client.as_ref(),
                &mut journal,
                &dom_render_compiler::aperture::WorkflowLimits::default(),
                self.build_id.as_ref(),
                durability.as_ref(),
                |seeded| {
                    // Cloned per pass, outside the async block, so the future
                    // owns everything and borrows neither `self` nor the pool.
                    let pool = Arc::clone(pool);
                    let project = self.project.clone();
                    let broadcast = self.live.broadcast.clone();
                    let envelope = envelope.clone();
                    // Two `Arc` bumps. The view must be cloned rather than moved
                    // because a suspended body will need it again.
                    let view = view.clone();

                    async move {
                        pool.with_engine(move |engine| {
                            // FORGE · the write collector is a THREAD-LOCAL and
                            // the body runs on the pool's own engine thread, so
                            // it must be installed inside this closure. Around
                            // the `await` it would leave an `append()` recording
                            // into a collector on the wrong thread — silently
                            // discarding a durable write.
                            //
                            // Installed per *pass*, which is what makes the
                            // discard rule work: a pass that suspends hands its
                            // intents back and `drive_workflow` drops them, on
                            // the same terms `__albedo_effects` is rebuilt.
                            let collector =
                                dom_render_compiler::forge::install_forge_write_collector();
                            let pass = project.invoke_action_quickjs_pass(
                                engine,
                                &envelope,
                                &view,
                                Some(broadcast.as_ref()),
                                Some(&seeded),
                            );
                            // `ForgeWrite` is plain data, so the recorded
                            // intents cross back over the thread boundary.
                            pass.map(|pass| (pass, collector.take())).map_err(|err| {
                                format!(
                                    "compiled action handler {action_id} (quickjs) failed: {err:#}"
                                )
                            })
                        })
                        .await
                        .map_err(|err| {
                            format!("engine pool checkout for action {action_id} failed: {err}")
                        })
                        .and_then(|inner| inner)
                        .map_err(dom_render_compiler::aperture::WorkflowError::Pass)
                    }
                },
            )
            .await
            .map_err(|err| RuntimeError::RequestHandling(err.to_string()))?;

            if !journal.is_empty() {
                debug!(
                    target: "albedo.aperture",
                    action_id,
                    steps = journal.len(),
                    "action completed a workflow"
                );
            }

            if !writes.is_empty() {
                let substrate = self.live.forge_substrate.get().ok_or_else(|| {
                    RuntimeError::RequestHandling(format!(
                        "compiled action handler {action_id} called append() but no FORGE \
                         substrate is wired; rebuild with --features forge"
                    ))
                })?;
                // S4 · the current row projector, cloned out of the live slot
                // so the borrow the write path passes outlives no lock guard.
                let projector = self.live.projector();
                let fan_out = dom_render_compiler::forge::apply_writes(
                    substrate.as_ref(),
                    self.live.broadcast.as_ref(),
                    self.live.forge_schema.as_ref(),
                    &writes,
                    projector.as_deref(),
                    ctx.principal.as_ref(),
                )
                .await
                .map_err(|err| {
                    RuntimeError::RequestHandling(format!(
                        "compiled action handler {action_id} FORGE write failed: {err}"
                    ))
                })?;
                // SHUTTER · report the blast radius back to the dispatcher, which
                // is the only layer that knows who to charge for it. See
                // `shutter::note_fan_out`.
                crate::shutter::note_fan_out(fan_out.subscribers);
            }

            // …and only now is the workflow finished.
            //
            // 🔑 **After the writes, never before.** The two are not in one
            // transaction, so one is second, and which one decides what a crash
            // between them costs: result-first loses the writes *silently*,
            // writes-first duplicates them *visibly*. Duplication is recoverable
            // and is also exactly what happens today with no result table at
            // all, so this ordering cannot regress anything. See
            // `JournalLedger::complete`.
            if identity.resumable {
                if let Some(ledger) = &ledger {
                    use dom_render_compiler::ir::wire::WireEncode;
                    if let Ok(bytes) = instructions.wire_encode() {
                        // A failure here costs a repeated submit's protection,
                        // not the submit — the effects are already durable.
                        if let Err(err) = ledger
                            .complete(
                                &identity.id,
                                self.build_id.as_ref(),
                                &bytes,
                                dom_render_compiler::aperture::now_ms(),
                            )
                            .await
                        {
                            tracing::warn!(
                                target: "albedo.aperture.ledger",
                                workflow = %identity.id,
                                error = %err,
                                "could not record the workflow result; a repeated submit \
                                 would run again"
                            );
                        }
                    }
                }
            }

            return Ok(instructions);
        }

        // FORGE · the durable write path. `invoke_action_with_forge` installs
        // the broadcast registry (as below) AND a write collector, so a body's
        // `append(collection, record)` records an intent instead of attempting
        // I/O from a synchronous evaluation. The intents are applied here,
        // where we are async: mutate → rematerialize → fan out, atomically.
        //
        // The body still runs when no substrate is wired; `append()` then fails
        // inside the body with a clear message rather than being silently
        // dropped, which is the only honest outcome for a durable write.
        if let Some(substrate) = self.live.forge_substrate.get() {
            let (instructions, writes) = self
                .project
                .invoke_action_with_forge(envelope, &view, self.live.broadcast.as_ref())
                .map_err(|err| {
                    RuntimeError::RequestHandling(format!(
                        "compiled action handler {} failed: {err:#}",
                        self.action_id
                    ))
                })?;

            // Applied AFTER the body returned Ok: a body that errored partway
            // never reaches here, so its earlier appends are discarded with it.
            let projector = self.live.projector();
            let fan_out = dom_render_compiler::forge::apply_writes(
                substrate.as_ref(),
                self.live.broadcast.as_ref(),
                self.live.forge_schema.as_ref(),
                &writes,
                projector.as_deref(),
                ctx.principal.as_ref(),
            )
            .await
            .map_err(|err| {
                RuntimeError::RequestHandling(format!(
                    "compiled action handler {} FORGE write failed: {err}",
                    self.action_id
                ))
            })?;
            crate::shutter::note_fan_out(fan_out.subscribers);

            return Ok(instructions);
        }

        // Phase P · C.2 — `invoke_action_with_broadcast` installs the
        // broadcast registry on the per-thread Phase K stack for the
        // duration of `eval_handler_body`, so a TS action body's
        // `broadcast(topic, updater)` call routes through this same
        // `Arc<BroadcastRegistry>`. Fan-out lands on every subscribed
        // session over the WT patches lane without further plumbing.
        self.project
            .invoke_action_with_broadcast(envelope, &view, self.live.broadcast.as_ref())
            .map_err(|err| {
                RuntimeError::RequestHandling(format!(
                    "compiled action handler {} failed: {err:#}",
                    self.action_id
                ))
            })
    }
}

type SharedHandler = Arc<dyn RouteHandler>;
type SharedApiHandler = Arc<dyn ApiHandler>;
type SharedLayoutHandler = Arc<dyn LayoutHandler>;
type SharedMiddleware = Arc<dyn RuntimeMiddleware>;
type SharedAuthProvider = Arc<dyn AuthProvider>;
type SharedPropsLoader = Arc<dyn PropsLoader>;

/// The self-contained render + dispatch state produced by one build. Held
/// behind an `RwLock<Arc<_>>` in [`RuntimeState`] so `albedo dev` can boot a
/// fresh world on a source change and swap it in atomically — the listening
/// socket, the HMR / error-overlay SSE connections, and the inspector all stay
/// live across the swap. `albedo serve` stores exactly one world and never
/// swaps it: the read lock is uncontended (the only writer is a dev file-save),
/// and loading is a single refcount bump on the render hot path.
///
/// Everything render-coupled lives here as one unit so a full-reload swap is
/// trivially consistent — the action handlers, their slot store, the CSRF table,
/// and the streaming state are always the ones built together.
struct RenderWorld {
    router: Arc<CompiledRouter>,
    handlers: Arc<HashMap<String, SharedHandler>>,
    /// Phase-F — API handlers keyed by the same `handler_id` namespace
    /// as page handlers. Dispatch picks the right registry by looking
    /// up `target.handler_id` here before falling through to `handlers`.
    api_handlers: Arc<HashMap<String, SharedApiHandler>>,
    /// Phase-G — action handlers keyed by `action_id` (the same u32
    /// `BindEvent.proxy_id` carries on the wire). Served via
    /// `POST /_albedo/action`.
    action_handlers: Arc<ActionRegistry>,
    /// Phase-H — shared reactive slot store. Action handlers read and
    /// write through a `SessionSlots` view built per-request; the
    /// pipeline (when bound) holds the same `Arc<SlotStore>` so writes
    /// are visible to both sides without copying.
    slot_store: Arc<SlotStore>,
    /// Phase L — per-session CSRF token registry. The streaming
    /// handler mints a token per page response and fills it into every
    /// hidden form input the renderers stamped; the action dispatcher
    /// validates submitted `_csrf` fields against this same map.
    csrf: Arc<CsrfRegistry>,
    /// Phase L — `action_id`s reachable from a form, and so required to
    /// present a valid CSRF token. Fixed at build time from the
    /// compiled project's forms plus every `register_form_action`, so
    /// the dispatcher never has to ask a request whether it ought to be
    /// checked.
    form_action_ids: Arc<FormActionIds>,
    /// AUTH § 8.1.3 — `action_id`s declared on a route whose `export const auth`
    /// is `"required"`. Derived from the manifest at build (see
    /// [`GatedActionIds`]), so the dispatcher answers *may this caller run this*
    /// from a table fixed before any request arrived, exactly as the CSRF set
    /// above is.
    gated_action_ids: Arc<GatedActionIds>,
    layouts: Arc<HashMap<String, SharedLayoutHandler>>,
    middleware: Arc<HashMap<String, SharedMiddleware>>,
    auth_provider: SharedAuthProvider,
    request_timeout: Duration,
    streaming_runtime: Option<Arc<StreamingAppState>>,
    /// Phase N — public/ static asset mount(s). When present,
    /// `dispatch` checks for a matching file before falling through
    /// to the dynamic route matcher.
    public_assets: Option<Arc<PublicAssets>>,
    /// Phase O.2 — broadcast slot registry. Topic-keyed shared
    /// state; writes fan out as `SlotSet` opcodes over the WT
    /// patches lane to every subscribed session. Always allocated
    /// (cheap when unused); userland reaches it via
    /// `AlbedoServer::broadcast()`.
    broadcast: Arc<BroadcastRegistry>,
    /// Tier C · Phase 2 — the content-hashed npm chunks Tier-C islands load.
    /// Lives on the world (not on the process) so a dev reload that changes an
    /// island's imports swaps the chunk table with everything else it swaps.
    npm_chunks: Arc<dom_render_compiler::bundler::client_npm::ClientNpmGraph>,
}

#[derive(Clone)]
struct RuntimeState {
    /// The live render world. Cloned once per request (a refcount bump); the
    /// guard is released immediately so nothing is held across an `.await`.
    /// Swapped wholesale by the dev reloader — `serve` never writes it.
    world: Arc<RwLock<Arc<RenderWorld>>>,
    /// Persists across a world swap so its `set_graph` heartbeat and any open
    /// inspector UI survive a dev reload.
    inspector: Option<Arc<InspectorState>>,
    /// Phase M.1 — error registry the floating overlay subscribes
    /// to. `None` in production builds; `Some` when dev mode is on.
    /// Persists across a world swap so the overlay's SSE stream isn't dropped
    /// and build errors from a failed reload can still reach it.
    dev_error_registry: Option<crate::dev::SharedErrorRegistry>,
    /// Phase M.2 — HMR registry the in-place DOM-swap client
    /// subscribes to. Same on/off semantics as the error registry.
    /// Persists across a world swap: the dev reloader pushes the reload event
    /// through the SAME registry the client's live SSE stream subscribed to.
    dev_hmr_registry: Option<crate::dev::SharedHmrRegistry>,
    /// Print per-request server-compute timings (ns/µs) to the terminal.
    /// A persistent server property (not part of the swappable `RenderWorld`),
    /// so a dev hot-swap keeps it. `true` for CLI dev/serve, `false` otherwise.
    request_timings: bool,
    /// The live runtime singletons — broadcast registry, FORGE substrate,
    /// row projector — held on the **persistent** tier so a dev world-swap
    /// never strands them. `run()` fills the substrate through this handle;
    /// the dispatcher reaches the projector through it for reconnect resync;
    /// and — the point of the whole bundle — a reload threads THIS instance
    /// into the fresh build so the swapped-in world reuses it rather than
    /// minting empties. See [`LiveRuntime`]. (The broadcast registry is also
    /// reachable via `world().broadcast`, which is the same `Arc`.)
    live: LiveRuntime,
    /// PHOSPHOR — the per-browser-profile lane table. Persistent (a trunk
    /// belongs to a browser, not to a page or a world): a dev hot-swap keeps
    /// open trunks, and new subscribes bind against whatever world is live
    /// at subscribe time. See `handlers::phosphor` + `development-plan/PHOSPHOR.md`.
    phosphor: Arc<crate::handlers::PhosphorState>,
    /// SHUTTER — the rate limiter and the trusted-proxy rule. Persistent for the
    /// same reason the broadcast registry is: a dev world swap replaces build
    /// output, and one that also reset every accumulated limit would hand out a
    /// fresh budget on every file save. See `development-plan/AUTH.md` R6.
    shutter: Arc<crate::shutter::Limiter>,
}

impl RuntimeState {
    /// Load the current render world. One refcount bump; the read guard is
    /// dropped before returning, so callers never hold it across an `.await`.
    fn world(&self) -> Arc<RenderWorld> {
        self.world
            .read()
            .expect("render world lock poisoned")
            .clone()
    }
}

pub struct AlbedoServerBuilder {
    config: AppConfig,
    handlers: HashMap<String, SharedHandler>,
    /// Phase-F — API handler registry. Distinct from `handlers` so
    /// dispatch can pick the right call path; same handler_id namespace
    /// so a route's `handler` field resolves to whichever registry the
    /// user populated.
    api_handlers: HashMap<String, SharedApiHandler>,
    /// Phase-G — action handler registry keyed by u32 `action_id`.
    /// Populated via [`Self::register_action`]; served by the
    /// `POST /_albedo/action` axum route.
    action_handlers: ActionRegistry,
    /// Phase L — the subset of `action_handlers` a form can submit to,
    /// which the dispatcher requires a CSRF token from. Fed by
    /// [`Self::register_form_action`] (explicit) and
    /// [`Self::register_compiled_project`] (every
    /// `<form action="action:NAME">` the compiler found).
    form_action_ids: FormActionIds,
    props_loaders: HashMap<String, SharedPropsLoader>,
    layouts: HashMap<String, SharedLayoutHandler>,
    middleware: HashMap<String, SharedMiddleware>,
    auth_provider: SharedAuthProvider,
    renderer: Option<RendererRuntime>,
    /// Dev inspector toggle. `Some(true)` / `Some(false)` overrides the
    /// default. `None` defaults to `cfg!(debug_assertions)` — on in
    /// debug builds, off in release.
    inspector_enabled: Option<bool>,
    /// Phase-E opcode registry. When set, the WT streaming path runs
    /// Tier-B render functions through this and ships opcodes; when
    /// unset, the WT path falls back to SSE.
    opcode_registry: Option<Arc<dyn TierBOpcodeRegistry>>,
    /// Phase-D opcode pipeline + tokio runtime handle. The handle is
    /// stashed alongside so the pipeline can spawn resolver Futures.
    /// Userland binds both via `with_pipeline`.
    pipeline: Option<(FourLaneRuntimePipeline, tokio::runtime::Handle)>,
    /// Phase M — dev-mode toggle. `Some(true)` / `Some(false)`
    /// overrides; `None` defaults to `cfg!(debug_assertions)` so
    /// debug builds get the overlay + HMR endpoints automatically.
    dev_mode_enabled: Option<bool>,
    /// Print each request's server-compute time (ns/µs) to the terminal.
    /// Off by default so library embedders + the test harness stay silent;
    /// `boot_production_server` flips it on for both `albedo dev` and
    /// `albedo serve`. See [`crate::timing`].
    request_timings_enabled: bool,
    /// Phase N — directories served verbatim at the URL root. Each
    /// `with_public_dir` call appends; the first matching root wins.
    public_dirs: Vec<std::path::PathBuf>,
    /// AUTH · the lowered `auth` block, waiting for a substrate.
    ///
    /// `None` until `boot.rs` supplies one; the resulting runtime resolves
    /// everybody as anonymous when no providers were declared.
    auth_registry: Option<dom_render_compiler::auth::AuthRegistry>,
    /// Phase N — `Cache-Control` value applied to every public asset
    /// response. `None` means auto: `public, max-age=3600` when dev
    /// mode is off, `no-store` when dev mode is on.
    public_cache_control: Option<String>,
    /// The runtime singletons — broadcast registry, FORGE substrate handle,
    /// row projector — shared into every `CompiledProjectActionAdapter` and the
    /// `RuntimeState`. Minted fresh by [`Self::new`]; a dev reload overrides it
    /// via [`Self::with_live_runtime`] so the rebuilt world reuses the live
    /// registry (topic values + subscribers) and the already-open substrate
    /// rather than stranding them. See [`LiveRuntime`].
    live: LiveRuntime,
    /// A1 · optional pool of warmed QuickJS engines. When set (via
    /// [`Self::with_quickjs_action_engine_pool`]), every adapter built by a
    /// *subsequent* [`Self::register_compiled_project`] runs its action bodies
    /// through the QuickJS executor instead of the pure-Rust interpreter,
    /// unlocking loops/`try`/array methods in handler bodies. `None` keeps the
    /// pure-Rust path. Order matters: enable the pool before registering the
    /// project, since the adapter captures the pool handle at registration.
    action_engine_pool: Option<Arc<crate::engine_pool::QuickJsEnginePool>>,
    /// Step 3 (binding mode) — the last [`CompiledProject`] registered, retained
    /// so [`Self::build`] can precompute fine-grained reactive blocks
    /// (`RendererRuntime::build_reactive_blocks`) for routes whose Tier-C
    /// components are driveable from text bindings alone. `None` keeps the A3
    /// whole-component island path for every route.
    reactive_project: Option<Arc<dom_render_compiler::runtime::CompiledProject>>,
    /// APERTURE A2 · the id every adapter stamps on the workflows it starts
    /// (§ 11 R8). Set from the manifest's `build_id` by [`Self::with_build_id`];
    /// otherwise a marker that names its own absence, because a build id that
    /// silently defaulted to `""` would compare equal across two builds and the
    /// check would pass by accident.
    build_id: Option<String>,
}

impl AlbedoServerBuilder {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            handlers: HashMap::new(),
            api_handlers: HashMap::new(),
            action_handlers: ActionRegistry::new(),
            form_action_ids: FormActionIds::new(),
            props_loaders: HashMap::new(),
            layouts: HashMap::new(),
            middleware: HashMap::new(),
            auth_provider: Arc::new(AllowAllAuthProvider),
            renderer: None,
            inspector_enabled: None,
            opcode_registry: None,
            pipeline: None,
            dev_mode_enabled: None,
            request_timings_enabled: false,
            public_dirs: Vec::new(),
            auth_registry: None,
            public_cache_control: None,
            // Minted empty; `register_compiled_project` clones the same handles
            // into every adapter, `run()` fills the substrate, `build()`
            // installs the projector. A dev reload replaces this whole bundle
            // via `with_live_runtime` so live state carries across the swap.
            live: LiveRuntime::new(),
            // A1 · off by default — opt in via `with_quickjs_action_engine_pool`.
            action_engine_pool: None,
            // Step 3 · set by `register_compiled_project`.
            reactive_project: None,
            // APERTURE · set by `with_build_id` from the manifest.
            build_id: None,
        }
    }

    /// APERTURE A2 · the build id workflows started by this server are stamped
    /// with (§ 11 R8).
    ///
    /// Pass the manifest's `build_id`: it is derived from the build's inputs, so
    /// it is the same across a restart of the same build and different across a
    /// rebuild — which is the property A3 needs when it resumes a journal it
    /// read back from disk.
    #[must_use]
    pub fn with_build_id(mut self, build_id: impl Into<String>) -> Self {
        self.build_id = Some(build_id.into());
        self
    }

    /// APERTURE · install the client an action body's `fetch()` goes out
    /// through, overriding the one [`Self::build`] would otherwise mint.
    ///
    /// Two callers: a test that wants a `CountingTransport` instead of a socket,
    /// and any embedder that wants its own egress policy. When a source reader
    /// is installed this is redundant — the reader's own client is adopted, so
    /// declared reads and bare calls share one policy.
    #[must_use]
    pub fn with_aperture_client(
        self,
        client: Arc<dom_render_compiler::aperture::ApertureClient>,
    ) -> Self {
        self.live.install_aperture_client(client);
        self
    }

    /// A1 · route compiled action bodies through a pool of warmed QuickJS
    /// engines instead of the pure-Rust interpreter. Spawns `size` engine
    /// threads (each warmed before this returns), so call it once, at boot.
    ///
    /// **Order matters:** enable the pool *before*
    /// [`Self::register_compiled_project`] — the adapter captures the pool
    /// handle at registration time, so projects registered earlier keep the
    /// pure-Rust path. A `size` of 0 is treated as 1.
    ///
    /// The QuickJS path runs the same broadcast-aware executor and ships the
    /// identical `SlotSet` wire shape as the pure-Rust path (proven at parity in
    /// `compiled_project_dispatch.rs`), but additionally tolerates JS the
    /// pure-Rust evaluator rejects (loops, `try`/`catch`, array methods).
    #[must_use]
    pub fn with_quickjs_action_engine_pool(mut self, size: usize) -> Self {
        self.action_engine_pool = Some(Arc::new(crate::engine_pool::QuickJsEnginePool::with_size(
            size,
        )));
        self
    }

    /// Phase P · C.2 — access the broadcast registry this builder
    /// will install on the eventual [`AlbedoServer`]. Useful when
    /// userland code needs to seed a topic (with
    /// [`BroadcastRegistry::topic`]) before any client connects.
    /// Cloning the returned `Arc` is cheap; both halves resolve to
    /// the same registry.
    pub fn broadcast(&self) -> Arc<BroadcastRegistry> {
        self.live.broadcast.clone()
    }

    /// Reuse an existing [`LiveRuntime`] instead of the empty one [`Self::new`]
    /// minted. This is the seam that makes `albedo dev` hot reload correct for
    /// FORGE: the dev reloader threads the running server's live bundle in, so
    /// the rebuilt world's adapters, streaming state, and topic pre-registration
    /// all resolve against the SAME broadcast registry (keeping hydrated topic
    /// values and open subscribers) and the SAME already-opened substrate.
    /// Must be called before [`Self::register_compiled_project`], which clones
    /// the bundle into every adapter.
    #[must_use]
    pub(crate) fn with_live_runtime(mut self, live: LiveRuntime) -> Self {
        self.live = live;
        self
    }

    /// AUTH · carry the lowered `auth` block toward the live runtime.
    ///
    /// Stored rather than installed, because an [`crate::auth::AuthRuntime`]
    /// needs the substrate and the substrate is not open until `run()`. Lowered
    /// in `boot.rs` for the same reason `sources` is — it needs the real
    /// environment, and a bad block must fail the boot naming the offending
    /// provider.
    #[must_use]
    pub(crate) fn with_auth_registry(
        mut self,
        registry: dom_render_compiler::auth::AuthRegistry,
    ) -> Self {
        self.auth_registry = Some(registry);
        self
    }

    /// APERTURE · install the declared-source read path onto the live runtime.
    ///
    /// Installed here rather than constructed in `build()` because lowering the
    /// `sources` block needs the real environment and must fail the *boot* with
    /// the offending source named — which `boot.rs` can do and this builder
    /// cannot. Idempotent: a dev reload passing the same live runtime keeps the
    /// reader it already has, cache and all.
    pub(crate) fn with_source_reader(
        self,
        reader: Option<Arc<dom_render_compiler::aperture::SourceReader>>,
    ) -> Self {
        if let Some(reader) = reader {
            self.live.install_source_reader(reader);
        }
        self
    }

    /// Phase N — mount a directory whose files are served verbatim
    /// at the URL root (`<dir>/logo.svg` → `GET /logo.svg`). Multiple
    /// calls stack; the first matching root wins. Lookups go through
    /// [`crate::handlers::public_assets::sanitize_public_path`] so
    /// traversal attempts cannot escape the mount.
    #[must_use]
    pub fn with_public_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.public_dirs.push(dir.into());
        self
    }

    /// Phase N — override the `Cache-Control` header used for public
    /// asset responses. When unset the value tracks dev mode:
    /// `no-store` in dev, `public, max-age=3600` in production.
    #[must_use]
    pub fn with_public_cache_control(mut self, value: impl Into<String>) -> Self {
        self.public_cache_control = Some(value.into());
        self
    }

    /// Phase M — explicit toggle for the error overlay + HMR
    /// surface mounted at `/_albedo/dev/*`. `None` (default) means
    /// auto: enabled on `cfg!(debug_assertions)`, off otherwise.
    #[must_use]
    pub fn with_dev_mode(mut self, enabled: bool) -> Self {
        self.dev_mode_enabled = Some(enabled);
        self
    }

    /// Install the app's FORGE collection registry, replacing the built-in
    /// guestbook default [`LiveRuntime::new`] mints.
    ///
    /// The schema is app-static: it is the `topic → (query, schema)` allowlist
    /// the write path resolves against, and it is fixed for the life of the
    /// process. Call this **before** [`Self::with_live_runtime`] — a dev reload
    /// passes the running server's `LiveRuntime` through that method, and
    /// reusing its schema (rather than rebuilding one) is what keeps the reload
    /// swapping build output without disturbing live state. The corollary is
    /// that editing the config's `forge` block needs a restart, not a reload.
    #[must_use]
    pub fn with_forge_schema(
        mut self,
        schema: dom_render_compiler::forge::ForgeSchema,
    ) -> Self {
        self.live.forge_schema = Arc::new(schema);
        self
    }

    /// Print each handled request's server-compute time (ns/µs) to stdout.
    /// The CLI (`albedo dev` / `albedo serve`) turns this on via
    /// [`crate::boot_production_server`]; library embedders opt in explicitly.
    /// Only page-render GETs and action POSTs are timed — static assets,
    /// framework JS, dev SSE streams, and the WT transport are skipped so the
    /// log is pure ALBEDO numbers. See [`crate::timing`].
    #[must_use]
    pub fn with_request_timings(mut self, enabled: bool) -> Self {
        self.request_timings_enabled = enabled;
        self
    }

    /// Forces the dev inspector on or off. By default the inspector is mounted
    /// when the binary is built with debug assertions and skipped otherwise —
    /// call this to override that policy (for example, to expose the inspector
    /// in a release-mode preview build).
    pub fn with_inspector(mut self, enabled: bool) -> Self {
        self.inspector_enabled = Some(enabled);
        self
    }

    pub fn register_handler(
        mut self,
        handler_id: impl Into<String>,
        handler: impl RouteHandler + 'static,
    ) -> Self {
        self.handlers.insert(handler_id.into(), Arc::new(handler));
        self
    }

    /// Registers an [`ApiHandler`] under `handler_id`. Routes whose
    /// `handler` field resolves to this id are dispatched through the
    /// API path ([`dispatch_api_route`]) instead of the page-route
    /// pipeline. Auth still flows through the registered
    /// `AuthProvider` against `RouteTarget.auth`.
    pub fn register_api_handler(
        mut self,
        handler_id: impl Into<String>,
        handler: impl ApiHandler + 'static,
    ) -> Self {
        self.api_handlers
            .insert(handler_id.into(), Arc::new(handler));
        self
    }

    /// Phase-G — registers an [`ActionHandler`] under the u32
    /// `action_id`. Bakabox's `BindEvent` opcode carries `action_id`
    /// as its `proxy_id`; when the corresponding DOM event fires, the
    /// client POSTs an `ActionEnvelope` to `/_albedo/action`. The
    /// handler returns opcode patches which the dispatcher wire-encodes
    /// and returns to bakabox for in-place DOM mutation.
    pub fn register_action(
        mut self,
        action_id: u32,
        handler: impl ActionHandler + 'static,
    ) -> Self {
        self.action_handlers.insert(action_id, Arc::new(handler));
        self
    }

    /// Phase K — register every handler in a [`CompiledProject`] into
    /// the action registry. This is the bridge that turns a successful
    /// compile + render into a live action dispatcher: bakabox POSTs
    /// `/_albedo/action` with the `proxy_id` it learned from a
    /// `BindEvent` opcode, the dispatcher routes by `action_id` (same
    /// `u32`), and the compiled handler body executes server-side via
    /// the shared Phase-J interpreter with setter calls translating to
    /// slot writes.
    ///
    /// The same `CompiledProject` instance should drive both rendering
    /// (`render_entry_with_bindings`) and dispatch (this builder
    /// method) so the slot ids, proxy ids, and handler bodies all line
    /// up. Multiple `CompiledProject`s can coexist by calling this
    /// method repeatedly — proxy_id collisions are vanishingly
    /// unlikely (FNV-1a-32 over `{module}::{fn}::{event}#{idx}`) but
    /// later registrations win.
    pub fn register_compiled_project(
        mut self,
        project: Arc<dom_render_compiler::runtime::CompiledProject>,
    ) -> Self {
        // Step 3 · retain for binding-mode precompute in `build()` (cheap Arc
        // clone; the same instance drives render bindings + action dispatch).
        self.reactive_project = Some(project.clone());

        // APERTURE · one allocation for the whole registration rather than one
        // per adapter. `<no-build-id>` is deliberately not the empty string: two
        // builds that both defaulted to `""` would compare equal and R8's check
        // would pass without ever having been configured.
        let build_id: Arc<str> = Arc::from(
            self.build_id
                .as_deref()
                .unwrap_or("<no-build-id>"),
        );

        for proxy_id in project.handler_proxy_ids() {
            let adapter = CompiledProjectActionAdapter {
                project: project.clone(),
                action_id: proxy_id,
                // A1 · when a pool was enabled (before this call), route action
                // bodies through QuickJS; otherwise the pure-Rust path. Cloning
                // an `Arc` — every adapter for this project shares one pool.
                engine_pool: self.action_engine_pool.clone(),
                // The shared live bundle: broadcast (so `broadcast(topic, fn)`
                // routes through the same registry the WT/SSE runtime sees),
                // the substrate cell `run()` fills, and the projector slot
                // `build()` installs. Reused across a dev reload.
                live: self.live.clone(),
                build_id: Arc::clone(&build_id),
            };
            self.action_handlers.insert(proxy_id, Arc::new(adapter));
        }

        // Phase L · every `<form action="action:NAME">` the compiler found in
        // this project's JSX names an action that MUST present a CSRF token.
        // Taking the set from the compiled project (rather than inferring it
        // per-request from the payload) is what lets the gate fail closed: a
        // form action with no token is rejected even if the renderer that
        // produced the form forgot to emit the input.
        self.form_action_ids.extend(project.form_action_ids());

        // Phase P · Stream C.3 — auto-register every `useSharedSlot`
        // topic this project references so the streaming handler's
        // C.4 auto-subscribe pass (and any userland `broadcast()`
        // write that happens before the first subscriber) finds a
        // live `BroadcastTopic` to attach to. `BroadcastRegistry::topic`
        // is idempotent — a second call with the same name returns
        // the existing entry rather than clobbering its value, so
        // calling this on multiple `CompiledProject`s that share
        // topics is safe. Seed value is `b"null"` rather than `b"[]"`
        // because we don't know the topic's element type at this
        // layer; the `broadcast()` interpreter builtin already
        // tolerates a `Null` current value by passing it to the
        // updater closure.
        for topic in project.shared_slot_topics() {
            // Idempotent: `topic()` returns an existing topic without touching
            // its value, so on a dev reload (reused registry) this never
            // clobbers a value FORGE already hydrated.
            self.live.broadcast.topic(topic, b"null".to_vec());
        }

        self
    }

    /// Phase L — registers a typed form-submit handler under an
    /// action **name** (the suffix the JSX form's
    /// `action="action:NAME"` carries). The builder derives the
    /// stable `action_id` via FNV-1a-32 (the same hash family the
    /// renderer stamps into `data-albedo-action`), so userland never
    /// has to compute the id by hand. The dispatcher decodes the
    /// incoming `ActionEnvelope.payload` as JSON into `T` before
    /// invoking `handler`; on parse failure the action surfaces a
    /// [`RuntimeError::RequestHandling`] which the action HTTP path
    /// renders as a 500 with the underlying serde message.
    ///
    /// The form payload shape is the JSON object the client-side
    /// runtime emits from a browser `FormData`: keys are input
    /// `name` attributes, values are the last submitted string value
    /// for each name. Repeated `name`s collapse to the last value
    /// (matches `<form>` POST semantics). For per-field validation
    /// patches (`SetText` opcodes targeting `data-albedo-error`
    /// spans), implement [`crate::render::FromFormPayload`] on a
    /// wrapping type and register through
    /// [`Self::register_action`] with [`crate::render::form_action_handler`].
    pub fn register_form_action<T, F, Fut>(
        mut self,
        action_name: impl Into<String>,
        handler: F,
    ) -> Self
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: Fn(RequestContext, T, SessionSlots) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<
                Output = Result<Vec<dom_render_compiler::ir::opcode::Instruction>, RuntimeError>,
            > + Send
            + 'static,
    {
        // Derive the wire-level `action_id` from the user-supplied
        // action name. Same FNV-1a-32 family the compile-time form
        // extractor uses, so the JSX `action="action:NAME"` and the
        // server-side `register_form_action("NAME", ...)` resolve to
        // the same `action_id` on the wire without any per-route
        // configuration.
        let action_name = action_name.into();
        let action_id = crate::render::form_action::form_action_id(&action_name);

        let handler = Arc::new(handler);
        let wrapped = move |ctx: RequestContext,
                            envelope: dom_render_compiler::ir::action::ActionEnvelope,
                            slots: SessionSlots| {
            let handler = handler.clone();
            async move {
                let parsed: T = serde_json::from_slice(&envelope.payload).map_err(|err| {
                    RuntimeError::RequestHandling(format!(
                        "form payload did not deserialize as {}: {err}",
                        std::any::type_name::<T>()
                    ))
                })?;
                (handler)(ctx, parsed, slots).await
            }
        };
        self.action_handlers.insert(action_id, Arc::new(wrapped));
        // Phase L · registering a handler *as a form action* is itself the
        // statement that a form submits to it, so the CSRF gate applies —
        // whether or not a compiled project also declares the form in JSX.
        self.form_action_ids.insert(action_id);
        self
    }

    pub fn register_props_loader(
        mut self,
        loader_id: impl Into<String>,
        loader: impl PropsLoader + 'static,
    ) -> Self {
        self.props_loaders
            .insert(loader_id.into(), Arc::new(loader));
        self
    }

    pub fn register_layout(
        mut self,
        layout_id: impl Into<String>,
        layout_handler: impl LayoutHandler + 'static,
    ) -> Self {
        self.layouts
            .insert(layout_id.into(), Arc::new(layout_handler));
        self
    }

    pub fn register_middleware(
        mut self,
        middleware_id: impl Into<String>,
        middleware: impl RuntimeMiddleware + 'static,
    ) -> Self {
        self.middleware
            .insert(middleware_id.into(), Arc::new(middleware));
        self
    }

    pub fn with_auth_provider(mut self, auth_provider: impl AuthProvider + 'static) -> Self {
        self.auth_provider = Arc::new(auth_provider);
        self
    }

    pub fn with_renderer_runtime(mut self, renderer: RendererRuntime) -> Self {
        self.renderer = Some(renderer);
        self
    }

    /// Registers the Phase-E opcode registry that resolves Tier-B
    /// nodes for the WT streaming path. Without it the WT streaming
    /// path errors out and the request falls back to SSE.
    pub fn with_opcode_registry(mut self, registry: impl TierBOpcodeRegistry + 'static) -> Self {
        self.opcode_registry = Some(Arc::new(registry));
        self
    }

    /// Binds an opcode pipeline + tokio runtime handle. The pair is
    /// installed on `StreamingAppState` so Phase-D's async-island
    /// machinery can spawn resolver Futures and Phase-E's WT path can
    /// drain opcode chunks. Pair this with [`Self::with_opcode_registry`]
    /// to enable the binary WT path end-to-end.
    pub fn with_pipeline(
        mut self,
        pipeline: FourLaneRuntimePipeline,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        self.pipeline = Some((pipeline, runtime_handle));
        self
    }

    pub fn build(self) -> Result<AlbedoServer, RuntimeError> {
        self.config.validate()?;

        // APERTURE · an action body's `fetch()` needs a client whether or not
        // the app declared any `sources` — a bare call is § 6's escape hatch, not
        // a declared read. `install_*` is idempotent, so a reader's client (or a
        // test's) already installed above wins and this is a no-op.
        //
        // Failing the boot rather than degrading: a server that could not build
        // an HTTP client is a server on which every outbound call will fail, and
        // discovering that on the first user click is strictly worse than
        // discovering it at startup.
        if self.live.aperture_client().is_none() {
            let mode = if self.dev_mode_enabled.unwrap_or(false) {
                dom_render_compiler::aperture::EgressMode::Dev
            } else {
                dom_render_compiler::aperture::EgressMode::Serve
            };
            let policy = Arc::new(dom_render_compiler::aperture::EgressPolicy::new(mode));
            let transport =
                dom_render_compiler::aperture::ReqwestTransport::new(Arc::clone(&policy)).map_err(
                    |err| {
                        RuntimeError::ServerStartup(format!(
                            "could not build the APERTURE outbound client: {err}"
                        ))
                    },
                )?;
            self.live.install_aperture_client(Arc::new(
                dom_render_compiler::aperture::ApertureClient::new(
                    Arc::new(transport),
                    Arc::new(dom_render_compiler::aperture::ResponseCache::new(
                        dom_render_compiler::aperture::DEFAULT_RESPONSE_BUDGET,
                    )),
                    policy,
                ),
            ));
        }

        let router = CompiledRouter::from_route_and_layout_specs(
            self.config.routes.as_slice(),
            self.config.layouts.as_slice(),
        )?;

        let mut renderer = self.renderer;
        if renderer.is_none() {
            if let Some(renderer_config) = &self.config.renderer {
                renderer = Some(RendererRuntime::from_config(renderer_config)?);
            }
        }

        let shared_wt_sessions = self
            .config
            .server
            .webtransport
            .enabled
            .then(WebTransportSessionRegistry::default);

        let mut services = SharedRenderServices {
            opcode_registry: self.opcode_registry.clone(),
            ..SharedRenderServices::default()
        };

        // A2 · the renderer's OWN engine needs the project's npm bundles too.
        //
        // 🪤 This is a third engine, and it was the one nobody wired. The pool
        // install below covers actions and Tier-B; `CompiledProject` covers the
        // compiled project. Neither is the engine `render_island_html` uses, so
        // a Tier-C island importing a package server-rendered **nothing** — an
        // empty placeholder — while its client chunk loaded perfectly. Tier C's
        // Phase 2 is what made that reachable, and a package that only works
        // once JavaScript runs is exactly the claim this framework denies.
        if let (Some(runtime), Some(project)) =
            (renderer.as_mut(), self.reactive_project.as_deref())
        {
            let artifacts: Vec<(String, String, u64)> = project
                .npm_bundles()
                .iter()
                .flat_map(|bundle| bundle.artifacts.iter())
                .map(|artifact| {
                    (
                        artifact.key.clone(),
                        artifact.script.clone(),
                        artifact.source_hash,
                    )
                })
                .collect();
            if !artifacts.is_empty() {
                let failed = runtime.install_npm_bundles(&artifacts);
                tracing::info!(
                    target: "albedo.renderer",
                    artifacts = artifacts.len(),
                    failed,
                    "registered npm bundles on the island renderer"
                );
            }
        }

        // RSC · Tier-B server rendering. The default `registry` is a stub that
        // returns empty markup, so async server components (and every legit
        // Tier-B island) render nothing on `albedo serve`. When both a renderer
        // and the warmed QuickJS action pool are present, swap in the pool-backed
        // registry: it resolves each Tier-B component's module graph at boot and
        // renders it through the same warmed/arena engines actions use, awaiting
        // any returned Promise on the server before lowering to HTML.
        if let (Some(runtime), Some(pool)) = (renderer.as_ref(), self.action_engine_pool.as_ref()) {
            // The compiled project supplies each Tier-B component's
            // `useSharedSlot` topics — the render manifest doesn't carry them,
            // and without them the request path can't resolve a shared slot.
            let plan = runtime.build_tier_b_render_plan(self.reactive_project.as_deref());

            // A2 · give every pool engine the project's npm bundles **before**
            // anything renders on them.
            //
            // 🔑 The order is load-bearing: warm-up below renders real
            // components, and a component module links its imports eagerly at
            // load, so warming first would fail on precisely the components
            // that import a package.
            //
            // Without this the pooled render path had no npm at all — the
            // Tier-B registry, the row projector and the warm-up each load only
            // the boot-precomputed *project* modules, and none of them goes
            // through `CompiledProject`, which is where `preload_npm_bundles`
            // lives. That is why actions could import a package and a
            // per-request component could not.
            if let Some(project) = self.reactive_project.as_deref() {
                let artifacts: Vec<crate::engine_pool::NpmArtifactRegistration> = project
                    .npm_bundles()
                    .iter()
                    .flat_map(|bundle| bundle.artifacts.iter())
                    .map(|artifact| {
                        (
                            artifact.key.clone(),
                            artifact.script.clone(),
                            artifact.source_hash,
                        )
                    })
                    .collect();
                if !artifacts.is_empty() {
                    let failed = pool.install_npm_bundles(&artifacts);
                    tracing::info!(
                        target: "albedo.renderer",
                        artifacts = artifacts.len(),
                        failed,
                        "registered npm bundles on the render pool"
                    );
                }
            }

            // Warm every pool engine's render path with the real Tier-B components
            // before the pool serves a request. The arena's O(1) reset is only safe
            // once a component's interned QuickJS state lives in the persistent
            // region; warming here (in persistent mode) puts it there, so the first
            // request-scoped render can't free-then-reuse it. Skipping this is the
            // crash, not a slow path.
            let warmup: Vec<crate::engine_pool::WarmupComponent> = plan
                .values()
                .map(|entry_plan| crate::engine_pool::WarmupComponent {
                    modules: entry_plan.modules.clone(),
                    entry: entry_plan.entry.clone(),
                    props_json: "{}".to_string(),
                })
                .collect();
            pool.warm_render_path(&warmup);

            tracing::info!(
                target: "albedo.renderer",
                tier_b_components = plan.len(),
                "installed pool-backed Tier-B render registry"
            );
            // The SAME broadcast `Arc` the action adapters and the WT/SSE
            // runtime use — and the one FORGE's boot hydration seeds topics
            // into. Handing the render path a different registry (or a
            // snapshot) would reintroduce the null-slot bug in a subtler form.
            // P6 · the per-action error-span markup the Tier-B shim appends to
            // its forms. Generated once here from the compiled project's form
            // manifest via the SAME `form_error_span_seed` the non-pooled render
            // path calls, so both emit identical sinks at the ids the submit
            // projection targets — no missing node, no dropped frame.
            let form_error_spans = self
                .reactive_project
                .as_deref()
                .map(|project| project.form_error_span_seed())
                .unwrap_or_default();
            // S4 · the same pool, plan and error spans back FORGE's row
            // projector, so a delta's row markup comes off the identical
            // template and engine a request would render it with. Installed
            // here rather than in `register_compiled_project` because the plan
            // does not exist until now; the adapters already hold this cell.
            self.live
                .install_projector(Arc::new(crate::render::PooledRowProjector::new(
                    pool.clone(),
                    plan.clone(),
                    form_error_spans.clone(),
                )));

            // PRISM · the warmer is the `LiveRuntime` itself — the persistent
            // tier holding the schema, the substrate and the registry. Cloning
            // it here (a handful of `Arc` bumps) rather than reaching through
            // the world is what keeps read-through materialisation working
            // across a dev reload: a rebuilt world has a fresh registry, and a
            // partition warmed into the previous one would be invisible.
            services.registry = Arc::new(
                PooledTierBRenderRegistry::new(
                    pool.clone(),
                    plan,
                    self.live.broadcast.clone(),
                    form_error_spans,
                    Arc::new(self.live.clone()),
                )
                // APERTURE · the registry comes from the same persistent tier as
                // the warmer, and for the same reason: a dev reload must not
                // re-mint it and discard a warm response cache.
                .with_sources(
                    self.live
                        .source_reader
                        .get()
                        .map(|reader| Arc::clone(reader.registry())),
                ),
            );
        }

        // Phase-H — one shared slot store for the lifetime of the
        // server. Action handlers read/write through it via the
        // dispatcher-built `SessionSlots`; the pipeline, when bound,
        // holds the same `Arc` so future tick-side emissions see the
        // same state. Without this sharing each side would run
        // against an empty store and the reactive loop never closes.
        let slot_store = Arc::new(SlotStore::new());

        // Phase L · mint the CSRF registry once and share the same
        // `Arc` between the streaming state (which mints tokens
        // during page render) and `RuntimeState` (which validates
        // them during action dispatch). The two paths MUST see the
        // same token table or every form POST 403s.
        let csrf_registry = Arc::new(CsrfRegistry::new());

        // Phase O.2 · single broadcast registry per server (minted in
        // the builder so `register_compiled_project` adapters share
        // the same `Arc`). Every route/action handler that publishes
        // a topic
        // ──────────────────────────────────────────────────────────
        // Phase P · C.2 trailing note: the same `Arc` is now reused
        // from `self.live.broadcast` rather than re-minted here, so
        // adapters registered before `build()` see the same registry
        // the runtime state ends up with. `subscribe()` / `write_topic()`
        // are themselves concurrent so no further sharing layer is
        // needed.
        let broadcast = self.live.broadcast.clone();

        // Construct StreamingAppState, binding the optional pipeline +
        // runtime handle when both are present. `with_pipeline` consumes
        // the pair, so `take()` to move it out of the builder. The Arc
        // wrap happens after pipeline binding so the bound pipeline is
        // visible through `state.pipeline()`.
        // A3 · precompute the per-route client-hydration blocks while the
        // (`!Send`) renderer is still single-threaded on the boot thread. The
        // resulting map is shared read-only into the streaming state so the
        // concurrent request path never touches the QuickJS engine.
        // Declared out here so the island failures survive the block that
        // computes them — they are boot output, not render output.
        let mut island_ssr_failures: Vec<crate::renderer_runtime::IslandRenderFailure> = Vec::new();
        let mut static_render_failures: Vec<
            dom_render_compiler::manifest::schema::StaticRenderFailure,
        > = Vec::new();
        let route_hydration = Arc::new({
            // Step 3 (binding mode) · build the fine-grained reactive blocks
            // FIRST (immutable borrow). For routes whose Tier-C component is
            // driveable from text bindings alone, this ships the Phase K static
            // HTML + inline driver. Each block records, via its placeholder ids,
            // exactly which islands it serve-wired.
            let reactive_blocks = match (renderer.as_ref(), self.reactive_project.as_ref()) {
                (Some(runtime), Some(compiled)) => runtime.build_reactive_blocks(compiled.as_ref()),
                _ => HashMap::new(),
            };
            // The placeholder ids each route already serve-wired — the A3 pass
            // skips these so it doesn't also emit an island for them.
            let claimed: HashMap<String, std::collections::HashSet<String>> = reactive_blocks
                .iter()
                .map(|(path, block)| {
                    (
                        path.clone(),
                        block
                            .placeholders
                            .iter()
                            .map(|(id, _)| id.clone())
                            .collect(),
                    )
                })
                .collect();

            // A3 · hydrate the islands the reactive pass did NOT claim.
            // Tier C · Phase 2 — the npm search root. Taken from the compiled
            // project rather than the dist directory, because that is the
            // directory whose `node_modules` the build resolved against; a
            // project with no compiled source has no npm to bundle and passes
            // `None`, which is the pre-Phase-2 refusal.
            let npm_root = self
                .reactive_project
                .as_deref()
                .map(|project| project.project().root().to_path_buf());
            let hydration_blocks = renderer
                .as_mut()
                .map(|runtime| runtime.build_hydration_blocks(&claimed, npm_root.as_deref()))
                .unwrap_or_default();
            // Carried out, not logged. An island that failed to SSR is absent
            // from its page, and the author has to be told by the CLI rather
            // than by a `tracing` subscriber that only exists under RUST_LOG.
            island_ssr_failures = renderer
                .as_mut()
                .map(RendererRuntime::take_island_ssr_failures)
                .unwrap_or_default();
            // Same argument, one tier up and one process earlier. A Tier-A
            // render is baked at BUILD time, so its failures cannot be
            // recomputed here — they ride in on the manifest, which is the only
            // thing that survives `albedo build` → `albedo serve`.
            static_render_failures = renderer
                .as_ref()
                .map(|runtime| runtime.manifest().static_render_failures.clone())
                .unwrap_or_default();

            // Fix #3 · merge per-component, not per-route, so a single route can
            // carry BOTH a binding-mode island and an A3-hydrated island.
            crate::renderer_runtime::merge_island_blocks(hydration_blocks, reactive_blocks)
        });

        // Resolved here (not at its original site below) because the streaming
        // state needs it to decide whether to inject the dev overlay/HMR client.
        let dev_mode_enabled = self.dev_mode_enabled.unwrap_or(cfg!(debug_assertions));

        let mut pipeline_binding = self.pipeline;
        let streaming_runtime = renderer.as_ref().map(|runtime| {
            let state = StreamingAppState::new(
                Arc::new(runtime.manifest().clone()),
                services.clone(),
                StreamingTransportConfig::new(
                    self.config.server.webtransport.enabled,
                    self.config.server.port,
                ),
                shared_wt_sessions.clone(),
            )
            .with_csrf(csrf_registry.clone())
            // Phase P · C.4 — same broadcast `Arc` the action adapter
            // and runtime state hold, so a WT session's auto-subscribe
            // attaches the patches-lane sender to topics that
            // subsequent action-handler `broadcast()` calls fan out to.
            .with_broadcast(broadcast.clone())
            .with_hydration(route_hydration.clone())
            .with_dev_mode(dev_mode_enabled);
            let state = match pipeline_binding.take() {
                Some((pipeline, handle)) => {
                    let pipeline = pipeline.with_slot_store(slot_store.clone());
                    state.with_pipeline(pipeline, handle)
                }
                None => state,
            };
            Arc::new(state)
        });

        // AUTH § 8.1.3 · derive the gated-action set from the manifest, once.
        //
        // Read off `RouteManifest.action_ids` — the record of which route module
        // each `action_id` was exported from — so an action's gate is the gate
        // its author already wrote on the route, and there is no second place to
        // declare (or forget) it.
        //
        // 🔑 Built here rather than in `with_compiled_project`, because the
        // question is about a *route*, and routes live in the manifest. The
        // compiled project knows which actions exist; only the manifest knows
        // where each one was written.
        //
        // A project with no gated route produces an empty set and the check
        // below is one hash lookup that always misses — which is the price of
        // it being unconditional rather than something a request could skip.
        let gated_action_ids: GatedActionIds = streaming_runtime
            .as_ref()
            .map(|runtime| {
                runtime
                    .manifest
                    .routes
                    .values()
                    .filter(|route| !route.auth.allows_anonymous())
                    .flat_map(|route| route.action_ids.iter().map(|entry| entry.action_id))
                    .collect()
            })
            .unwrap_or_default();

        let has_entry_routes = self
            .config
            .routes
            .iter()
            .any(|route| route.entry_module.is_some());

        for route in &self.config.routes {
            let has_layout_handlers = match router.match_route(route.method, route.path.as_str()) {
                RouteMatch::Matched(matched) => !matched.target.layout_handlers.is_empty(),
                RouteMatch::MethodNotAllowed { .. } | RouteMatch::NotFound => true,
            };

            let route_uses_manifest_streaming =
                matches!(route.method, HttpMethod::Get | HttpMethod::Head)
                    && route.entry_module.is_some()
                    && route.props_loader.is_none()
                    && route.auth.is_none()
                    && route.middleware.is_empty()
                    && !has_layout_handlers
                    && streaming_runtime
                        .as_ref()
                        .map(|runtime| runtime.manifest.routes.contains_key(route.path.as_str()))
                        .unwrap_or(false);

            // Phase-F: a route's `handler` may resolve to either a
            // page `RouteHandler` or an API `ApiHandler`. Build fails
            // only when neither registry knows the id.
            if !route_uses_manifest_streaming
                && !self.handlers.contains_key(route.handler.as_str())
                && !self.api_handlers.contains_key(route.handler.as_str())
            {
                return Err(RuntimeError::HandlerNotFound {
                    handler_id: route.handler.clone(),
                });
            }
            if let Some(props_loader_id) = &route.props_loader {
                if !self.props_loaders.contains_key(props_loader_id) {
                    return Err(RuntimeError::PropsLoaderNotFound {
                        loader_id: props_loader_id.clone(),
                    });
                }
            }
            for middleware in &route.middleware {
                if !self.middleware.contains_key(middleware.as_str()) {
                    return Err(RuntimeError::MiddlewareNotFound {
                        middleware_id: middleware.clone(),
                    });
                }
            }
        }
        if has_entry_routes && renderer.is_none() {
            return Err(RuntimeError::RendererNotConfigured);
        }
        for layout in &self.config.layouts {
            if !self.layouts.contains_key(layout.handler.as_str()) {
                return Err(RuntimeError::LayoutNotFound {
                    layout_id: layout.handler.clone(),
                });
            }
        }

        let inspector_enabled = self.inspector_enabled.unwrap_or(cfg!(debug_assertions));
        let inspector = if inspector_enabled {
            let inspector_state = Arc::new(InspectorState::new());
            if let Some(streaming) = streaming_runtime.as_ref() {
                inspector_state.set_graph(InspectorGraphSnapshot::from_manifest(
                    streaming.manifest.as_ref(),
                ));
            }
            Some(inspector_state)
        } else {
            None
        };

        // Phase M · mint dev-mode registries when enabled. `dev_mode_enabled`
        // was resolved earlier (the streaming state needs it); defaults follow
        // the inspector convention (on in debug builds, off in release) so a
        // `cargo run --release` server doesn't leak dev routes.
        let (dev_error_registry, dev_hmr_registry) = if dev_mode_enabled {
            (
                Some(Arc::new(crate::dev::DevErrorRegistry::new())),
                Some(Arc::new(crate::dev::HmrRegistry::new())),
            )
        } else {
            (None, None)
        };

        let public_assets = if self.public_dirs.is_empty() {
            None
        } else {
            let cache_control = self.public_cache_control.unwrap_or_else(|| {
                if dev_mode_enabled {
                    "no-store".to_string()
                } else {
                    "public, max-age=3600".to_string()
                }
            });
            Some(Arc::new(PublicAssets::new(
                self.public_dirs,
                cache_control.as_str(),
            )))
        };

        let world = RenderWorld {
            router: Arc::new(router),
            handlers: Arc::new(self.handlers),
            api_handlers: Arc::new(self.api_handlers),
            action_handlers: Arc::new(self.action_handlers),
            slot_store,
            // Phase L · same Arc the streaming state holds, so
            // tokens minted during page render are the ones the
            // action dispatcher validates against.
            csrf: csrf_registry.clone(),
            form_action_ids: Arc::new(self.form_action_ids),
            gated_action_ids: Arc::new(gated_action_ids),
            layouts: Arc::new(self.layouts),
            middleware: Arc::new(self.middleware),
            auth_provider: self.auth_provider,
            request_timeout: Duration::from_millis(self.config.server.request_timeout_ms),
            streaming_runtime,
            public_assets,
            broadcast,
            // Built above by `build_hydration_blocks`; empty for a project whose
            // islands import no package, in which case no page emits a chunk tag
            // and the dispatch arm is one failed lookup.
            npm_chunks: Arc::new(
                renderer
                    .as_ref()
                    .map(|runtime| runtime.client_npm().clone())
                    .unwrap_or_default(),
            ),
        };

        let state = RuntimeState {
            world: Arc::new(RwLock::new(Arc::new(world))),
            inspector,
            dev_error_registry,
            dev_hmr_registry,
            request_timings: self.request_timings_enabled,
            // The same live bundle the adapters and streaming state hold. `run()`
            // fills its substrate; `build()` (above) installed its projector.
            // Carrying THIS instance — not a fresh one — is what lets a dev
            // reload reuse it (via `with_live_runtime`) instead of stranding the
            // hydrated topics and open subscribers.
            live: self.live,
            phosphor: Arc::new(crate::handlers::PhosphorState::new()),
            // SHUTTER · built here so a limits configuration that could never
            // admit its own heaviest operation fails the build rather than
            // surfacing later as one endpoint that 429s at every instant — a
            // symptom indistinguishable from load. See `Limits::check_admits_heaviest`.
            shutter: Arc::new(
                crate::shutter::Limiter::from_env()
                    .map_err(|err| RuntimeError::ServerStartup(format!("SHUTTER: {err}")))?,
            ),
        };

        Ok(AlbedoServer {
            config: self.config,
            state,
            auth_registry: self.auth_registry,
            island_ssr_failures,
            static_render_failures,
        })
    }
}

pub struct AlbedoServer {
    config: AppConfig,
    state: RuntimeState,
    /// AUTH · the lowered `auth` block, installed onto the live runtime by
    /// `run()` once the substrate it resolves against is open.
    auth_registry: Option<dom_render_compiler::auth::AuthRegistry>,
    /// Islands that failed to server-render during `build()`, held until
    /// `run_with_ready` can hand them to the [`BootReport`]. Boot decides them;
    /// only the readiness callback can print them.
    island_ssr_failures: Vec<crate::renderer_runtime::IslandRenderFailure>,
    /// Tier-A components whose build-time render failed, read off the manifest
    /// in `build()` and held for the same reason: boot decides them, only the
    /// readiness callback can print them.
    static_render_failures: Vec<dom_render_compiler::manifest::schema::StaticRenderFailure>,
}

/// What a boot changed on the author's behalf, handed to the readiness callback.
///
/// Only durable, author-visible side effects belong here — things someone would
/// want to know happened to a file they own. Routine startup work does not.
/// Empty on every boot that changed nothing, which is nearly all of them.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BootReport {
    /// Columns added to existing FORGE tables to match an edited `forge` block.
    /// See [`evolve_schema`](dom_render_compiler::forge::evolve_schema).
    pub schema_additions: Vec<dom_render_compiler::forge::Addition>,
    /// Islands whose SSR render threw. Each one is **missing from the page it
    /// belongs to** — an empty placeholder carries no `data-albedo-island`
    /// marker, so nothing ever hydrates into it.
    ///
    /// 🪤 This belongs here rather than in a log, and the distinction has cost
    /// this project twice. `install_tracing` in the CLI says it plainly: a
    /// subscriber exists only when `RUST_LOG` is set, so "a message on a
    /// channel nobody is listening to is indistinguishable from no message at
    /// all". A `<Link>` inside an island once removed an entire navigation bar
    /// from a real site with a green build and a clean console; the fix at the
    /// time raised `warn!` to `error!`, on the same unheard channel, and a
    /// Radix dialog hit the identical silence months later.
    pub island_ssr_failures: Vec<crate::renderer_runtime::IslandRenderFailure>,
    /// Tier-A components whose **build-time** render failed. Each one is
    /// missing from every page that renders it — and so is every Tier-A
    /// ancestor whose markup it was nested inside, because the evaluator's
    /// error propagates to the top of the static render.
    ///
    /// 🪤 Same lesson as `island_ssr_failures` above, learned separately and
    /// more expensively. This path did not merely go quiet: it fell back to
    /// scraping the component's own source file for the text between `<` and
    /// `>`, which put `);}` — the tail of a route's `.tsx` — into the served
    /// HTML with every tag stripped, under a green build and a clean console.
    /// The scrape is gone; what is left is this list.
    pub static_render_failures: Vec<dom_render_compiler::manifest::schema::StaticRenderFailure>,
}

impl BootReport {
    /// One human-readable line per change, in the order they were applied.
    ///
    /// Lives here rather than in each lane so `serve`, `dev` and the dashboard
    /// cannot drift into describing the same event three different ways.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .schema_additions
            .iter()
            .map(|addition| format!("FORGE · {addition}"))
            .collect();
        // Phrased as absence, because that is what happened. "failed to render"
        // reads like a degraded page; the component is simply not on it.
        out.extend(self.island_ssr_failures.iter().map(|failure| {
            format!(
                "ISLAND · {} is MISSING from every page that renders it. {}",
                failure.module_path, failure.error
            )
        }));
        // Phrased by the failure itself, not here. `albedo build` prints the
        // same event at the moment it happens, and item 6.5's whole point is
        // that one event does not get three wordings.
        out.extend(
            self.static_render_failures
                .iter()
                .map(dom_render_compiler::manifest::schema::StaticRenderFailure::report_line),
        );
        out
    }
}

impl AlbedoServer {
    /// The service, without connect info.
    ///
    /// ⚠️ **Serving this directly leaves SHUTTER unable to tell callers apart.**
    /// The peer address arrives as a request extension that only
    /// `into_make_service_with_connect_info` installs, so a router mounted
    /// without it rations every anonymous caller through one shared bucket (see
    /// `shutter::UNATTRIBUTED` — strict rather than absent, which is the right
    /// direction to be wrong in, but it is not what a real deployment wants).
    /// [`run`](Self::run) does the right thing; this exists for tests and for
    /// embedders composing their own stack, who should add the layer themselves.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/", any(dispatch))
            .route("/{*path}", any(dispatch))
            .with_state(self.state.clone())
            .layer(compression_layer())
    }

    /// Handle on the dev inspector's shared state, when one is mounted.
    /// Subsystems that want to publish render events into the inspector hold
    /// onto this `Arc` and call `publish_event` directly — there is no
    /// additional indirection from this method.
    pub fn inspector(&self) -> Option<Arc<InspectorState>> {
        self.state.inspector.clone()
    }

    /// Phase L · handle on the shared CSRF token registry. Used by
    /// integration tests that need to mint or inspect tokens
    /// outside the page-render path (for example, to construct a
    /// known-valid form-submit payload without first hitting the
    /// streaming handler). Production code does not need this — the
    /// page-render path mints tokens on its own.
    pub fn csrf_registry(&self) -> Arc<CsrfRegistry> {
        self.state.world().csrf.clone()
    }

    /// Phase M.1 · access the dev error overlay registry. `None`
    /// when the server was built without dev mode enabled. Userland
    /// integration code (a file watcher, an external linter, etc.)
    /// uses this to push errors into the in-browser overlay.
    pub fn dev_error_registry(&self) -> Option<crate::dev::SharedErrorRegistry> {
        self.state.dev_error_registry.clone()
    }

    /// Phase M.2 · access the slot-preserving HMR registry. Same
    /// availability rules as the error registry above.
    pub fn dev_hmr_registry(&self) -> Option<crate::dev::SharedHmrRegistry> {
        self.state.dev_hmr_registry.clone()
    }

    /// Phase N · expose the public asset registry for tests and
    /// userland code that wants to introspect the mounted roots.
    /// `None` when no `with_public_dir(..)` calls were made.
    pub fn public_assets(&self) -> Option<Arc<PublicAssets>> {
        self.state.world().public_assets.clone()
    }

    /// Phase O.2 · handle on the per-server broadcast registry.
    /// Route handlers, action handlers, and userland watchers all
    /// resolve topics against this `Arc`. Always available — there
    /// is no "broadcast disabled" mode; an unused registry is just
    /// an empty `DashMap` and costs nothing at idle.
    pub fn broadcast(&self) -> Arc<BroadcastRegistry> {
        self.state.world().broadcast.clone()
    }

    /// Hand the `albedo dev` file-watcher a handle to hot-swap the render world.
    /// `None` when dev mode is off (a hardened `albedo serve`), so the reload
    /// machinery is impossible to reach against a production server.
    pub fn dev_reload_handle(&self) -> Option<DevReloadHandle> {
        // Gate on the HMR registry — its presence IS the "dev mode on" signal,
        // and the handle needs it to notify clients.
        self.state.dev_hmr_registry.as_ref()?;
        Some(DevReloadHandle {
            world: self.state.world.clone(),
            hmr: self.state.dev_hmr_registry.clone(),
            errors: self.state.dev_error_registry.clone(),
            // The running server's live singletons. Threading these into every
            // rebuild is what makes a hot reload keep FORGE working: the fresh
            // world reuses the hydrated broadcast registry (values + open
            // subscribers) and the already-open substrate instead of stranding
            // them behind a swapped-out world.
            live: self.state.live.clone(),
            revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Boot and serve until shutdown.
    ///
    /// Every startup failure — an unopenable `forge.db`, a schema the database
    /// disagrees with, a port already bound — returns `Err` from here without
    /// ever serving a request.
    ///
    /// Discards the [`BootReport`]. Use [`run_with_ready`](Self::run_with_ready)
    /// from anything a person is watching.
    pub async fn run(self) -> Result<(), RuntimeError> {
        self.run_with_ready(|_| {}).await
    }

    /// [`run`](Self::run), with a signal for the moment the server is actually
    /// up.
    ///
    /// `on_ready` fires exactly once, after every fallible startup step has
    /// succeeded and the listener is accepting — and **never** if any of them
    /// failed. That ordering is the whole point: a caller that prints "serving"
    /// or opens a browser before this has fired is announcing a server that may
    /// not exist, which turns a diagnosable startup failure into what looks like
    /// a crash after a successful boot.
    ///
    /// It receives a [`BootReport`] describing what the boot changed. The caller
    /// is the only party that knows how to reach this user — a plain lane
    /// prints, the dashboard lane posts a note — so the report is handed over
    /// rather than logged. Nothing initialises a `tracing` subscriber in the
    /// shipped CLI, which makes `info!` a way of telling no one.
    pub async fn run_with_ready<F>(self, on_ready: F) -> Result<(), RuntimeError>
    where
        F: FnOnce(&BootReport) + Send + 'static,
    {
        let mut report = BootReport::default();
        // Decided at build time, surfaced here — this is the only path out to
        // the author.
        report.island_ssr_failures = self.island_ssr_failures.clone();
        report.static_render_failures = self.static_render_failures.clone();
        // FORGE — open the durable substrate exactly once, before the listener
        // binds. This is the sole async boot seam (`build()` is synchronous),
        // and the handle lives on the persistent `RuntimeState` tier so a dev
        // hot-swap never reopens `forge.db`. Gate-1 topic hydration and the
        // durable write path both hang off the handle stored here.
        #[cfg(feature = "forge")]
        {
            use dom_render_compiler::forge::{self, LibSqlSubstrate};

            let opened = LibSqlSubstrate::open_local("forge.db")
                .await
                .map_err(|err| {
                    RuntimeError::ServerStartup(format!("FORGE: failed to open forge.db: {err}"))
                })?;
            let substrate: Arc<dyn forge::DataSubstrate> = Arc::new(opened);

            // Gate 1 — hand-authored schema, then materialise every
            // FORGE-backed shared-slot topic from the substrate into the
            // broadcast registry BEFORE the listener binds. The register-time
            // seed leaves these topics at `b"null"`; hydration overwrites them
            // so the first SSR render reads persisted rows, not the placeholder.
            let schema = self.state.live.forge_schema.as_ref();
            // Before anything else: reconcile the database with what we are
            // about to serve. Migrations are `IF NOT EXISTS`, so an edited
            // `forge` block would otherwise apply as silence and surface later
            // as missing columns and failing writes — indistinguishable, from
            // the outside, from losing the data. A new nullable column is added
            // here; any other disagreement refuses the boot naming the field.
            // The refusal message is written for the author, so it is passed
            // through without a prefix of its own.
            // Carried out to the caller rather than logged here. Silently
            // altering someone's database is the failure mode this whole path
            // exists to eliminate, and applying the *correct* migration without
            // saying so is still not saying so.
            report.schema_additions = forge::drift::evolve_schema(substrate.as_ref(), schema)
                .await
                .map_err(|err| RuntimeError::ServerStartup(format!("FORGE: {err}")))?;
            forge::skeleton::bootstrap_schema(substrate.as_ref(), schema)
                .await
                .map_err(|err| {
                    RuntimeError::ServerStartup(format!("FORGE: schema bootstrap failed: {err}"))
                })?;
            forge::skeleton::hydrate_topics(
                substrate.as_ref(),
                self.state.world().broadcast.as_ref(),
                schema,
            )
            .await
            .map_err(|err| {
                RuntimeError::ServerStartup(format!("FORGE: topic hydration failed: {err}"))
            })?;

            // A3 · the workflow log's own table. Created here rather than
            // lazily on first use so a dispatch never pays a DDL round trip,
            // and so a substrate that cannot host it fails the boot rather than
            // the first outbound call.
            dom_render_compiler::aperture::JournalLedger::new(substrate.as_ref())
                .migrate()
                .await
                .map_err(|err| {
                    RuntimeError::ServerStartup(format!(
                        "APERTURE: workflow log migration failed: {err}"
                    ))
                })?;

            // …and the sweep that keeps it from growing forever.
            //
            // 🔑 A `sweep()` nobody calls is the bug this codebase keeps paying
            // for: a correct mechanism with no consumer. The table is
            // append-only by construction, so without this it is the same
            // unbounded-growth failure `DEFAULT_STEP_CAP` guards *inside* one
            // workflow, one level up and with no ceiling at all.
            //
            // Spawned here because this is async context — `boot_production_server`
            // itself runs outside the runtime, so a `tokio::spawn` there would
            // panic rather than schedule anything.
            {
                use dom_render_compiler::aperture::{
                    JournalLedger, DEFAULT_RETENTION, DEFAULT_SWEEP_INTERVAL,
                };
                let swept = Arc::clone(&substrate);
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(DEFAULT_SWEEP_INTERVAL);
                    // The first tick fires immediately, which is what clears
                    // whatever the previous process left behind.
                    loop {
                        ticker.tick().await;
                        let cutoff = dom_render_compiler::aperture::now_ms()
                            .saturating_sub(
                                i64::try_from(DEFAULT_RETENTION.as_millis()).unwrap_or(i64::MAX),
                            );
                        match JournalLedger::new(swept.as_ref()).sweep(cutoff).await {
                            Ok(0) => {}
                            Ok(rows) => tracing::debug!(
                                target: "albedo.aperture.ledger",
                                rows,
                                "swept expired workflow steps"
                            ),
                            // A failed sweep is a growing table, not a broken
                            // request path — worth saying, not worth stopping for.
                            Err(err) => tracing::warn!(
                                target: "albedo.aperture.ledger",
                                error = %err,
                                "workflow log sweep failed; the table will keep growing"
                            ),
                        }
                    }
                });
            }

            // Fill the cell every action adapter is already holding a clone of.
            // Before the listener binds, so no request can race an empty cell —
            // and `set` cannot fail here (nothing else writes it). Held on the
            // persistent `LiveRuntime`, so a later dev reload reuses this exact
            // handle rather than reopening the database or serving an empty one.
            let _ = self.state.live.forge_substrate.set(substrate);
            info!("FORGE substrate opened (forge.db); topics hydrated; writes enabled");
        }

        // AUTH · install the identity path, now that the substrate it resolves
        // against is open. Before the listener binds, so no request can reach a
        // half-installed runtime and resolve as anonymous when it should not
        // have — a race that would look like an intermittent logout.
        //
        // An app that declared no providers still gets a runtime: it answers
        // "anonymous" without spending a query, which keeps the request path
        // free of an `Option` that only one kind of app would ever fill.
        if let Some(registry) = self.auth_registry.clone() {
            match self.state.live.forge_substrate.get() {
                Some(substrate) => {
                    let providers = registry.providers.len();
                    self.state
                        .live
                        .install_auth(Arc::new(crate::auth::AuthRuntime::new(
                            registry,
                            Arc::clone(substrate),
                        )));
                    if providers > 0 {
                        info!("AUTH: {providers} provider(s) declared; sessions enabled");
                    }
                }
                // Declaring providers with no substrate is not a warning, it is
                // a broken app: every login would write to a database that is
                // not there, and the failure would surface as a login that
                // silently does nothing.
                None if !registry.is_empty() => {
                    return Err(RuntimeError::ServerStartup(
                        "the `auth` block declares providers but no FORGE substrate is open — \
                         sessions, users and credentials are FORGE rows, so auth cannot run \
                         without one"
                            .to_string(),
                    ));
                }
                None => {}
            }
        }

        let addr = self.config.server.socket_addr()?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| RuntimeError::ServerStartup(err.to_string()))?;
        info!("ALBEDO server listening on {}", addr);
        // SHUTTER · the trust question, answered out loud. Zero trusted proxies
        // behind a load balancer is a misconfiguration whose only symptom is
        // over-strict limiting — the whole internet arriving as one address —
        // and an operator chasing that needs to be able to find this line.
        info!(
            target: "albedo.shutter",
            trusted_proxies = self.state.shutter.trusted_proxies(),
            "SHUTTER active; set {} when running behind a load balancer",
            crate::shutter::TRUSTED_PROXIES_ENV
        );
        let router = self.router();

        let shutdown_timeout = Duration::from_millis(self.config.server.shutdown_timeout_ms);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        if let Some(inspector_state) = self.state.inspector.clone() {
            info!("ALBEDO dev inspector mounted at /__albedo");
            crate::inspector::heartbeat::spawn(inspector_state, shutdown_rx.clone());
        }

        // APERTURE · the thing that asks. Without it a declared source is a
        // shared cache and nothing more: a viewer sitting on an open tab issues
        // no render and no subscribe, so nothing consults the cache, so nothing
        // ever refreshes. Spawned here rather than at install time because this
        // is where a runtime and a shutdown signal both exist — and binding it
        // to `shutdown_rx` is what stops a dev reload, which stands up a new
        // server over the same broadcast registry, from accumulating one loop
        // per reload.
        if let Some(reader) = self.state.live.source_reader() {
            let refresher = dom_render_compiler::aperture::RefreshLoop::new(
                self.state.live.broadcast.clone(),
                reader.clone(),
            );
            let shutdown = shutdown_rx.clone();
            info!(
                "APERTURE refresh loop active ({}s tick)",
                dom_render_compiler::aperture::DEFAULT_TICK.as_secs()
            );
            tokio::spawn(async move {
                refresher
                    .run(dom_render_compiler::aperture::DEFAULT_TICK, shutdown)
                    .await;
            });
        }

        let webtransport_task = if self.config.server.webtransport.enabled {
            let world = self.state.world();
            let shared_sessions = world
                .streaming_runtime
                .as_ref()
                .and_then(|streaming| streaming.webtransport_sessions.clone())
                .unwrap_or_default();
            let runtime = WebTransportRuntime::bind_with_registry(
                addr,
                &self.config.server.webtransport,
                shared_sessions,
            )?
            .with_broadcast(world.broadcast.clone());
            info!("ALBEDO WebTransport QUIC listener active on {}", addr);
            let wt_shutdown = shutdown_rx.clone();
            Some(tokio::spawn(async move { runtime.run(wt_shutdown).await }))
        } else {
            info!("ALBEDO WebTransport disabled; SSE/HTTP streaming fallback remains active");
            None
        };

        let graceful_shutdown = {
            let shutdown_tx = shutdown_tx.clone();
            async move {
                shutdown_signal(shutdown_timeout).await;
                let _ = shutdown_tx.send(true);
            }
        };

        // Everything that can fail a boot has now succeeded: the substrate is
        // open and agrees with the schema, the TCP listener is bound, and the
        // QUIC listener (if enabled) is too. Only from here is it true to tell
        // anyone the server is up.
        on_ready(&report);

        // SHUTTER · `into_make_service_with_connect_info` is what puts the peer
        // address in front of the limiter. Without it every anonymous request in
        // the process shares one bucket — see `shutter::UNATTRIBUTED`, which is
        // the deliberate answer for an embedder that mounts `router()` itself,
        // not something the serve path should ever rely on.
        let http_result = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(graceful_shutdown)
            .await
            .map_err(|err| RuntimeError::ServerRuntime(err.to_string()));

        let _ = shutdown_tx.send(true);

        if let Some(task) = webtransport_task {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(err) => {
                    return Err(RuntimeError::ServerRuntime(format!(
                        "webtransport task join failed: {err}"
                    )));
                }
            }
        }

        http_result
    }
}

/// Cloneable handle the `albedo dev` file-watcher uses to hot-swap the render
/// world after a rebuild, without knowing anything about [`RenderWorld`]'s
/// internals. It closes over the SAME world slot the running server dispatches
/// against, so a swap is visible to every subsequent request immediately — the
/// socket, the HMR/overlay SSE connections, and the inspector all stay live.
#[derive(Clone)]
pub struct DevReloadHandle {
    world: Arc<RwLock<Arc<RenderWorld>>>,
    hmr: Option<crate::dev::SharedHmrRegistry>,
    errors: Option<crate::dev::SharedErrorRegistry>,
    /// The running server's live singletons, threaded into every rebuild so a
    /// hot reload swaps *build output* without discarding *live state*.
    live: LiveRuntime,
    revision: Arc<std::sync::atomic::AtomicU64>,
}

impl DevReloadHandle {
    /// Rebuild the render world from disk and swap it in atomically, then push a
    /// hard-reload event to every connected HMR client.
    ///
    /// On build failure the LIVE world is left untouched and the error is
    /// surfaced to the overlay + returned, so a broken save degrades to "last
    /// good page, with the error shown" instead of a dead server. The fresh
    /// world is self-contained (router, handlers, action registry, streaming
    /// state, slot store — all built together), so grafting it on is trivially
    /// consistent; the fresh server's own dev registries are dropped and the
    /// persistent ones this handle holds carry the SSE streams across the swap.
    ///
    /// The rebuild is threaded the running server's [`LiveRuntime`], so the
    /// fresh world's adapters, streaming state, and topic pre-registration all
    /// resolve against the SAME broadcast registry (keeping hydrated topic
    /// values and open subscribers) and the SAME already-open FORGE substrate.
    /// Without this, every hot reload minted an empty registry + unfilled
    /// substrate cell — the guestbook rendered nothing and `append()` failed
    /// after the first save.
    pub fn reload(&self, opts: &crate::boot::ProductionServerOptions) -> Result<(), RuntimeError> {
        let fresh = crate::boot::boot_production_server_reusing(opts, self.live.clone())
            .inspect_err(|err| {
                self.report_build_error(err.to_string());
            })?;
        let new_world = fresh.state.world();
        *self.world.write().expect("render world lock poisoned") = new_world;

        let revision = self
            .revision
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if let Some(errors) = &self.errors {
            errors.clear();
        }
        if let Some(hmr) = &self.hmr {
            hmr.reload(revision);
        }
        Ok(())
    }

    /// Surface a build failure to the in-browser overlay without swapping the
    /// world (the last good render keeps serving). Used by the watcher when the
    /// rebuild step itself fails before `boot_production_server` is even reached.
    pub fn report_build_error(&self, message: impl Into<String>) {
        if let Some(errors) = &self.errors {
            errors.report(
                crate::dev::ErrorKind::Compile,
                message,
                None,
                None,
                None,
                None,
            );
        }
    }
}

/// Top-level axum entry point. Runs the real dispatch in a separate tokio
/// task so a panicking handler surfaces as a 500 rather than a dropped
/// connection.
/// gzip for every text response, and **never** for an event stream.
///
/// Without this the server ships its client JS uncompressed — ~96 KB across
/// `runtime.js`, `link-forms.js` and `wt-bootstrap.js` on a page that reports
/// its cost in kilobytes, which made the reported number and the transferred
/// number two unrelated things.
///
/// The SSE exclusion is spelled out rather than inherited from
/// [`DefaultPredicate`], whose membership has moved between `tower-http`
/// releases. Compressing `text/event-stream` is not a performance question: the
/// encoder holds bytes until it has a worthwhile block, so a `patch` frame
/// would sit in the compressor instead of reaching the browser, and every live
/// lane in the system (PHOSPHOR trunk, per-tab patches, dev overlay/HMR) rides
/// that content type. Streamed **HTML** is still compressed — the encoder
/// flushes per polled chunk, so Tier-B injection chunks keep arriving
/// progressively.
fn compression_layer() -> CompressionLayer<And<DefaultPredicate, NotForContentType>> {
    CompressionLayer::new()
        .compress_when(DefaultPredicate::new().and(NotForContentType::const_new(
            "text/event-stream",
        )))
}

async fn dispatch(State(state): State<RuntimeState>, request: Request<Body>) -> Response {
    // SHUTTER · the peer address, read as an extension rather than through the
    // `ConnectInfo` extractor so its absence is a value and not a 500. It is
    // absent exactly when the router was mounted without connect info — an
    // in-process test, or an embedder calling `router()` directly — and
    // `Limiter::key` has a defined, strict answer for that. Reading it here also
    // keeps `dispatch_inner`'s signature honest about needing it.
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0.ip());

    match tokio::task::spawn(dispatch_inner(state, peer, request)).await {
        Ok(response) => response,
        Err(join_err) => {
            let msg = if join_err.is_panic() {
                let payload = join_err.into_panic();
                payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("(unknown panic payload)")
                    .to_owned()
            } else {
                format!("task cancelled: {join_err}")
            };
            error!(cause = %msg, "request handler panicked — returning 500");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// SHUTTER · charge this request, and refuse it if the budget is spent.
///
/// Returns the [`Key`] that was charged so a caller who learns the real price
/// later can settle up against the same bucket (the action path, whose fan-out
/// is not knowable until the write resolves).
///
/// `identity` is passed rather than resolved because the branches differ on
/// purpose: the render, action and subscribe paths have already paid for an
/// identity lookup and should ration the *actor*, while a static asset has not
/// and must not — an indexed session query per image on the page is a real cost
/// for an answer that branch never reads. An asset therefore rations by address,
/// which is the correct subject for an unauthenticated byte stream anyway.
fn ration(
    state: &RuntimeState,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    identity: &crate::auth::Identity,
    cost: Cost,
    rationed: &mut Option<Verdict>,
) -> Result<Key, Response> {
    let key = state.shutter.key(identity, peer, headers, cost.class);
    let verdict = state.shutter.charge(&key, cost);
    if !verdict.is_admitted() {
        debug!(
            target: "albedo.shutter",
            class = cost.class.as_str(),
            why = %cost.explain(),
            "request refused"
        );
        return Err(crate::shutter::too_many_requests(&verdict));
    }
    *rationed = Some(verdict);
    Ok(key)
}

async fn dispatch_inner(
    state: RuntimeState,
    peer: Option<IpAddr>,
    request: Request<Body>,
) -> Response {
    let mut rationed: Option<Verdict> = None;
    let mut response = dispatch_routed(&state, peer, request, &mut rationed).await;
    // SHUTTER · budget headers on **admitted** responses too, not only on
    // refusals. A client that first learns its budget when it has already run out
    // cannot pace itself, which is how a well-behaved integration becomes a
    // thundering herd. Refusals carry their own headers already; this is the one
    // place the successful path gets them, which is why the verdict is threaded
    // out rather than stamped at each of the branches below.
    if let Some(verdict) = &rationed {
        crate::shutter::stamp(response.headers_mut(), verdict);
    }
    response
}

async fn dispatch_routed(
    state: &RuntimeState,
    peer: Option<IpAddr>,
    request: Request<Body>,
    rationed: &mut Option<Verdict>,
) -> Response {
    // Start the server-compute clock at the very top so the reported number
    // includes routing (the perfect-hash matcher is ours to claim) — but not a
    // byte of network. Only page-render GETs and action POSTs read it back out
    // (`crate::timing`); every other branch below returns without timing.
    let started = Instant::now();

    let method = match HttpMethod::try_from(request.method()) {
        Ok(method) => method,
        Err(err) => return err.into_response(),
    };

    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);

    // Load the live render world ONCE for this request so a concurrent dev
    // hot-swap can't split a single request across two worlds. Persistent state
    // (inspector, dev registries) is read straight off `state`.
    let world = state.world();

    if path == "/_albedo/wt" {
        if let Some(streaming_runtime) = &world.streaming_runtime {
            // A live lane is charged once, at open, not per frame: what it costs
            // to establish is a subscribe, and what it costs to feed is already
            // priced on the writer's side as fan-out.
            if let Err(refusal) = ration(
                state,
                peer,
                request.headers(),
                &crate::auth::Identity::Anonymous,
                Cost::flat(OperationClass::Read),
                rationed,
            ) {
                return refusal;
            }
            return streaming_handler(State(streaming_runtime.clone()), request)
                .await
                .into_response();
        }
    }

    // S4 · the patches lane. A page on the SSE transport has no WebTransport
    // session, and `auto_subscribe` used to live only on the WT connect path —
    // so a broadcast write reached zero subscribers and the open page went
    // stale until a reload. This subscribes the connection to exactly the
    // topics ITS route declares: the client sends the page path, the server
    // resolves it through the same router and manifest the render used, and
    // the client never gets to name a topic itself.
    if path == "/_albedo/patches" {
        // `Last-Event-ID` is the browser's own reconnect marker: EventSource
        // echoes the id of the last event it saw. Its presence is how this
        // distinguishes a client coming BACK (which may have missed deltas
        // while disconnected, and whose keyed lists a plain `SlotSet` seed
        // cannot repair) from one connecting for the first time (whose rows
        // are already in the HTML that just rendered).
        let reconnecting = request.headers().contains_key("last-event-id");
        // AUTH item 5 P1 · resolve the principal here too, and for the reason the
        // dispatcher gives: one request, one answer to *who is this*. This lane
        // is the SSE fallback for the same page the render served, so if it
        // resolved anonymously while the render resolved a principal, a
        // signed-in user would see their rows on load and then never see an
        // update to them — live data that is silently only-on-reload, which is
        // the worst shape of all because it looks like it works.
        let identity = state.live.identity(request.headers()).await;
        if let Err(refusal) = ration(
            state,
            peer,
            request.headers(),
            &identity,
            Cost::flat(OperationClass::Read),
            rationed,
        ) {
            return refusal;
        }
        let response = match &world.streaming_runtime {
            Some(streaming_runtime) => {
                let page_path = crate::routing::parse_query_string(query.as_deref())
                    .get("p")
                    .and_then(|values| values.first().cloned())
                    .unwrap_or_else(|| "/".to_string());
                let topics = resolve_route_topics(
                    &world,
                    streaming_runtime,
                    page_path.as_str(),
                    identity.principal().map(|who| &who.id),
                )
                .unwrap_or_default();
                // Only a reconnect needs a resync, and only the current
                // projector can produce it. Cloned out of the live slot into an
                // owned local so the borrow survives the `.await` below without
                // holding the lock.
                let projector_arc = reconnecting.then(|| state.live.projector()).flatten();
                let projector = projector_arc.as_deref();
                match streaming_runtime.broadcast() {
                    Some(broadcast) => {
                        crate::handlers::serve_patch_stream(broadcast.clone(), &topics, projector)
                            .await
                            .into_response()
                    }
                    // No registry wired: nothing can ever publish, so an empty
                    // stream is the truthful answer, not an error.
                    None => empty_patch_stream().await,
                }
            }
            None => empty_patch_stream().await,
        };
        if state.request_timings {
            crate::timing::print_request(method.as_str(), &path, started.elapsed());
        }
        return response;
    }

    // PHOSPHOR · the trunk — one SSE connection per browser profile; tabs
    // attach as route-scoped circuits via the subscribe POST below. The lane
    // table lives on persistent state, so a dev world-swap keeps open trunks.
    if path == "/_albedo/phosphor" && method == HttpMethod::Get {
        let identity = crate::render::csrf::read_session_cookie(request.headers());
        // AUTH · R2 — the live lane learns *who*, not just *which tab*.
        //
        // `identity` above is the tab: a `SessionId` minted for anyone who
        // visits, with no login involved. The principal is a different fact, and
        // the lane needs it because a subscribe grants topics — so from P1 on,
        // "which topics may this connection name" is a question about the human,
        // not the tab. Resolved here, at trunk open, so every route the lane
        // subscribes to over its lifetime is judged against one identity rather
        // than re-resolved per subscribe.
        let principal = state.live.identity(request.headers()).await;

        // Rationed against the principal when there is one — a trunk is per
        // browser profile, so an address bucket would make one office share one
        // browser's budget.
        if let Err(refusal) = ration(
            state,
            peer,
            request.headers(),
            &principal,
            Cost::flat(OperationClass::Read),
            rationed,
        ) {
            return refusal;
        }

        // `?dev=1` merges the overlay/HMR event streams onto the trunk so a
        // dev browser holds exactly one connection. Inert in production: the
        // registries are `None`, so the flag has nothing to stream.
        let wants_dev = crate::routing::parse_query_string(query.as_deref())
            .get("dev")
            .is_some();
        let dev = if wants_dev {
            crate::handlers::phosphor::DevTap {
                errors: state.dev_error_registry.clone(),
                hmr: state.dev_hmr_registry.clone(),
            }
        } else {
            crate::handlers::phosphor::DevTap::none()
        };
        let response = crate::handlers::phosphor::serve_trunk(
            state.phosphor.clone(),
            dev,
            identity,
            principal,
        )
        .await;
        if state.request_timings {
            crate::timing::print_request(method.as_str(), &path, started.elapsed());
        }
        return response;
    }

    // PHOSPHOR · the subscribe delta. Route→topic resolution and (future)
    // authorization go through `WorldRouteAuthority` — the single choke point
    // item 4's dynamic topics land into. Resolution binds against the world
    // that is live NOW, not the one the trunk opened under.
    if path == "/_albedo/phosphor/routes" && method == HttpMethod::Post {
        // A subscribe resolves a route's topics and **warms** any cold partition
        // it names, which is an indexed range scan per partition. That is a read
        // that reaches FORGE, so it is priced as one.
        if let Err(refusal) = ration(
            state,
            peer,
            request.headers(),
            &crate::auth::Identity::Anonymous,
            Cost::flat(OperationClass::Read),
            rationed,
        ) {
            return refusal;
        }
        let (_parts, body) = request.into_parts();
        let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
            Ok(body) => body,
            Err(err) => {
                return RuntimeError::RequestBodyRead(err.to_string()).into_response();
            }
        };
        let authority = WorldRouteAuthority {
            world: world.clone(),
            projector: state.live.projector(),
            live: state.live.clone(),
        };
        let response = crate::handlers::phosphor::handle_subscribe(
            state.phosphor.clone(),
            &authority,
            body.as_ref(),
        )
        .await;
        if state.request_timings {
            crate::timing::print_request(method.as_str(), &path, started.elapsed());
        }
        return response;
    }

    if inspector_routes::matches_inspector_path(path.as_str()) {
        if let Some(inspector) = &state.inspector {
            return inspector_routes::dispatch(inspector, path.as_str()).into_response();
        }
    }

    // Phase M · dev-mode error overlay + HMR endpoints. Only mounted
    // when the corresponding registries exist on RuntimeState; in
    // production builds both are None and these routes fall through
    // to the regular router, which surfaces a clean 404.
    if path.starts_with("/_albedo/dev/") {
        match path.as_str() {
            "/_albedo/dev/overlay.js" => {
                if state.dev_error_registry.is_some() {
                    return crate::handlers::dev::serve_overlay_script().into_response();
                }
            }
            "/_albedo/dev/hmr-apply.js" => {
                if state.dev_hmr_registry.is_some() {
                    return crate::handlers::dev::serve_hmr_apply_script().into_response();
                }
            }
            "/_albedo/dev/stream.js" => {
                if state.dev_error_registry.is_some() || state.dev_hmr_registry.is_some() {
                    return crate::handlers::dev::serve_dev_stream_script().into_response();
                }
            }
            // The merged stream — one connection carrying both event
            // names. This is what the injected clients use; the two
            // single-purpose routes below are kept so an existing tab
            // (or anything in userland that wired itself to them)
            // keeps working, but nothing we ship subscribes to them.
            "/_albedo/dev/stream" => {
                if state.dev_error_registry.is_some() || state.dev_hmr_registry.is_some() {
                    return crate::handlers::dev::serve_dev_stream(
                        state.dev_error_registry.clone(),
                        state.dev_hmr_registry.clone(),
                    )
                    .into_response();
                }
            }
            "/_albedo/dev/errors" => {
                if let Some(registry) = &state.dev_error_registry {
                    return crate::handlers::dev::serve_error_stream(registry.clone())
                        .into_response();
                }
            }
            "/_albedo/dev/hmr" => {
                if let Some(registry) = &state.dev_hmr_registry {
                    return crate::handlers::dev::serve_hmr_stream(registry.clone())
                        .into_response();
                }
            }
            _ => {
                if state.dev_error_registry.is_some() || state.dev_hmr_registry.is_some() {
                    return crate::handlers::dev::dev_not_found().into_response();
                }
            }
        }
    }

    // AUTH · P2 — the first-party sign-in endpoints. Placed ahead of everything
    // else under `/_albedo/` because they are the one surface an anonymous
    // stranger is *supposed* to reach, and because the limiting they need is not
    // the limiting the rest of this function applies.
    if let Some(auth_route) = crate::handlers::auth_routes::match_auth_route(path.as_str()) {
        use crate::handlers::auth_routes::{AuthRequest, AuthRoute};

        if method != HttpMethod::Post {
            // A credential never travels in a URL, so `GET` is not a slower way
            // to do this — it is the mistake that puts a password in the access
            // log, and it is refused rather than redirected.
            return ResponsePayload::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "sign-in endpoints accept POST only".to_string(),
            )
            .with_header("allow", "POST")
            .into_response();
        }

        let Some(auth) = state.live.auth() else {
            return RuntimeError::RouteNotFound {
                method: method.as_str().to_string(),
                path,
            }
            .into_response();
        };
        let auth = auth.clone();
        let identity = state.live.identity(request.headers()).await;
        let caller = state.shutter.key(
            &identity,
            peer,
            request.headers(),
            OperationClass::Credential,
        );

        // Registration and logout are rationed here, before the body is read.
        // **Login deliberately is not** — its limiter needs the account being
        // attempted, which is inside the body, so `run_auth_route` charges it
        // once the address is known. What bounds the pre-limit work is the read
        // cap below: an unauthenticated flood costs at most one small buffer per
        // request, and never the KDF, which is the expensive part.
        if auth_route != AuthRoute::PasswordLogin {
            let cost = match auth_route {
                AuthRoute::PasswordRegister => Cost::flat(OperationClass::Credential),
                _ => Cost::flat(OperationClass::Write),
            };
            if let Err(refusal) = ration(state, peer, request.headers(), &identity, cost, rationed)
            {
                return refusal;
            }
        }

        let (parts, body) = request.into_parts();
        let body = match to_bytes(body, crate::forms::MAX_FORM_BODY_BYTES).await {
            Ok(body) => body,
            Err(err) => return RuntimeError::RequestBodyRead(err.to_string()).into_response(),
        };

        // The **tab** session, not a login: a stranger arriving at the sign-in
        // page has one, and the CSRF token on the form they were served is bound
        // to it. Falling back to a fresh id means the token cannot validate,
        // which is the correct failure — a form submitted with no tab session
        // did not come from a page we rendered.
        let session = crate::render::csrf::read_session_cookie(&parts.headers)
            .unwrap_or_else(SessionId::random);

        let response = crate::handlers::auth_routes::run_auth_route(
            auth_route,
            AuthRequest {
                auth: auth.as_ref(),
                csrf: world.csrf.as_ref(),
                session,
                identity: &identity,
                headers: &parts.headers,
                body,
                shutter: state.shutter.as_ref(),
                caller,
            },
        )
        .await;

        if state.request_timings {
            crate::timing::print_request(method.as_str(), &path, started.elapsed());
        }
        return response;
    }

    // The no-JS form submit — `POST /_albedo/action/{name}`. Placed before the
    // envelope branch because it is the more specific path, and matched on the
    // segment rather than on a router pattern so the whole action surface stays
    // visible in one place.
    //
    // 🔑 **The action is named by the request line**, so this branch knows what
    // it is dispatching before it reads a byte of the body — which is what
    // `AUTH.md` § "P2's shape" asks for and what the bincode envelope below can
    // never offer.
    if method == HttpMethod::Post {
        if let Some(action_name) = form_action_segment(path.as_str()) {
            let principal = state.live.identity(request.headers()).await;

            // Priced exactly as the envelope path: same admission cost, same
            // post-hoc fan-out surcharge. A cheaper no-JS path would be a
            // limiter bypass wearing a progressive-enhancement costume.
            let key = match ration(
                state,
                peer,
                request.headers(),
                &principal,
                Cost::flat(OperationClass::Write),
                rationed,
            ) {
                Ok(key) => key,
                Err(refusal) => return refusal,
            };

            let (response, fan_out) = crate::shutter::metered(run_form_action_route(
                &world,
                state.dev_error_registry.as_ref(),
                request,
                action_name,
                principal,
            ))
            .await;

            if fan_out > 0 {
                state
                    .shutter
                    .debit(&key, Cost::surcharge(OperationClass::Write, fan_out));
            }
            if state.request_timings {
                crate::timing::print_request(method.as_str(), &path, started.elapsed());
            }
            return response;
        }
    }

    // Phase-G — bakabox → server action invocations land here. Only
    // POST is accepted; other methods fall through to the normal
    // router (which will surface 405 or 404 as appropriate).
    if path == "/_albedo/action" && method == HttpMethod::Post {
        // AUTH · resolved before the body is read so the handler is dispatched
        // under a known identity rather than acquiring one partway through.
        // Per-branch rather than once at the top of the dispatcher: a static
        // asset request carries the same cookies, and paying an indexed lookup
        // for every image on the page would be a real cost for an answer
        // nothing on that path reads.
        let principal = state.live.identity(request.headers()).await;

        // SHUTTER · an action is charged in two parts, because its price is only
        // half knowable in advance.
        //
        // **Admission** is the flat write cost: an action runs a user-authored
        // body against the substrate, and refusing it here is the only refusal
        // that saves any work at all.
        //
        // **The surcharge** is its blast radius, and no amount of analysis at
        // this line can produce it — a write to a partitioned collection lands on
        // a channel whose name depends on the record, and an update that moves a
        // row across partitions touches two. The write path resolves that inside
        // its transaction and reports it back through the meter below.
        let key = match ration(
            state,
            peer,
            request.headers(),
            &principal,
            Cost::flat(OperationClass::Write),
            rationed,
        ) {
            Ok(key) => key,
            Err(refusal) => return refusal,
        };

        let (response, fan_out) = crate::shutter::metered(run_action_route(
            &world,
            state.dev_error_registry.as_ref(),
            request,
            principal,
        ))
        .await;

        // Settled unconditionally, and deliberately after the fact: the write is
        // committed and a limiter does not un-commit one. What this buys is that
        // the *next* request is priced by what this one actually reached, which
        // is the difference between a derived weight and a guessed one. See
        // `Shutter::debit` for why it cannot go through `charge`.
        if fan_out > 0 {
            state
                .shutter
                .debit(&key, Cost::surcharge(OperationClass::Write, fan_out));
        }
        if state.request_timings {
            crate::timing::print_request(method.as_str(), &path, started.elapsed());
        }
        return response;
    }

    // Phase P · post-P wire-through — embedded bakabox client
    // assets. Serves runtime.js / bincode.js / link-forms.js etc.
    // from the binary directly, so production no longer needs to
    // mount `<dist>` as a public_dir (which used to shadow `/` with
    // the static fallback index.html). Fires BEFORE the
    // public-assets dispatch so a user's `public/runtime.js`
    // doesn't accidentally hijack the framework path.
    if matches!(method, HttpMethod::Get | HttpMethod::Head) {
        // Tier C · Phase 2 — a content-hashed npm chunk. Checked with the other
        // framework assets, and before `public/`, so nothing a user drops in
        // their static directory can shadow a package the page depends on.
        if let Some(response) = crate::handlers::albedo_assets::dispatch_client_npm_chunk(
            world.npm_chunks.as_ref(),
            path.as_str(),
        ) {
            if let Err(refusal) = ration(
                state,
                peer,
                request.headers(),
                &crate::auth::Identity::Anonymous,
                Cost::flat(OperationClass::StaticRead),
                rationed,
            ) {
                return refusal;
            }
            let mut response = response;
            if method == HttpMethod::Head {
                *response.body_mut() = Body::empty();
            }
            return response;
        }
        if let Some(response) = crate::handlers::albedo_assets::dispatch_albedo_asset(path.as_str())
        {
            // Charged after the lookup rather than before it: the lookup is a
            // match against a static table, and charging first would price every
            // request in the system as an asset on the way past.
            if let Err(refusal) = ration(
                state,
                peer,
                request.headers(),
                &crate::auth::Identity::Anonymous,
                Cost::flat(OperationClass::StaticRead),
                rationed,
            ) {
                return refusal;
            }
            let mut response = response;
            if method == HttpMethod::Head {
                *response.body_mut() = Body::empty();
            }
            return response;
        }
    }

    // Phase N — `public/` static assets resolve before dynamic
    // routes so `public/logo.svg` reliably serves at `/logo.svg`
    // even when the route map has a catch-all. GET/HEAD only; other
    // methods fall through and surface 405 from the router.
    if matches!(method, HttpMethod::Get | HttpMethod::Head) {
        if let Some(assets) = &world.public_assets {
            if let Some(file) = assets.resolve(path.as_str()) {
                if let Err(refusal) = ration(
                    state,
                    peer,
                    request.headers(),
                    &crate::auth::Identity::Anonymous,
                    Cost::flat(OperationClass::StaticRead),
                    rationed,
                ) {
                    return refusal;
                }
                let mut response = assets.read_response(&file);
                if method == HttpMethod::Head {
                    *response.body_mut() = Body::empty();
                }
                return response;
            }
        }
    }

    let route_match = world.router.match_route(method, path.as_str());

    // SHUTTER · what this route answers to, derived from what the build recorded
    // about it rather than from its path. A route that declares no live topics is
    // a cached render and is priced as one; a route that reads a FORGE partition
    // or an APERTURE source reaches the substrate on every request and is not.
    // **That distinction is the whole differentiation**: a limiter that sees only
    // a path and an address cannot make it, so every number it enforces has to
    // cover the worst case of both.
    //
    // An unmatched path is charged too, at the static rate. A 404 flood is still
    // a flood, and leaving the cheapest branch unrationed would make it the one
    // an attacker uses.
    let matched = match route_match {
        RouteMatch::Matched(matched) => matched,
        unmatched => {
            if let Err(refusal) = ration(
                state,
                peer,
                request.headers(),
                &crate::auth::Identity::Anonymous,
                Cost::flat(OperationClass::StaticRead),
                rationed,
            ) {
                return refusal;
            }
            return match unmatched {
                RouteMatch::MethodNotAllowed { allowed } => ResponsePayload::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    format!("method '{}' is not allowed for this route", method.as_str()),
                )
                .with_header(
                    "allow",
                    allowed
                        .iter()
                        .map(|method| method.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
                .into_response(),
                // `NotFound`, and `Matched` which the arm above took.
                _ => RuntimeError::RouteNotFound {
                    method: method.as_str().to_string(),
                    path,
                }
                .into_response(),
            };
        }
    };
    let class = matched_route_class(&world, &matched, method, path.as_str());

    // AUTH · resolved once for the whole matched branch, and handed to whichever
    // arm needs it. One request has one answer to *who is this*: a render whose
    // head was built for a stranger and whose body was built for somebody is
    // precisely the cross-principal bleed invariant 2.2 forbids, and two lookups
    // are two chances to disagree. Costs nothing when no session cookie was
    // presented — `AuthRuntime::resolve` returns without spending a query.
    let identity = state.live.identity(request.headers()).await;
    if let Err(refusal) = ration(
        state,
        peer,
        request.headers(),
        &identity,
        Cost::flat(class),
        rationed,
    ) {
        return refusal;
    }

    if should_use_manifest_streaming(&world, &matched.target, method, path.as_str()) {
        if let Some(streaming_runtime) = &world.streaming_runtime {
            // The manifest is keyed by route *pattern* (`/essays/[slug]`),
            // which `boot_production_server` mirrors into `entry_module`.
            // Pass that key plus the params `CompiledRouter` already
            // extracted so dynamic routes stream their async body + head.
            let route_pattern = matched
                .target
                .entry_module
                .clone()
                .unwrap_or_else(|| path.clone());
            let params: HashMap<String, String> = matched
                .params
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();

            // AUTH § 4 · the route's own gate, discharged before the render runs.
            //
            // Placed here rather than inside the renderer because a refusal must
            // cost nothing: the point of gating a route is to not do the work,
            // and a check inside the render has already built the shell. It also
            // reads the identity resolved above, so the gate and the body agree
            // about who is asking by construction rather than by two lookups.
            //
            // 🔑 This is a *route* gate, not the data boundary. An identity-keyed
            // read is already unreachable for a stranger — it resolves to no
            // topic — so what this adds is the case derivation cannot reach: a
            // route over global data that should still require signing in. See
            // `RouteAuth` for why it is authored rather than derived.
            let gated = streaming_runtime
                .manifest
                .routes
                .get(route_pattern.as_str())
                .is_some_and(|route| !route.auth.allows_anonymous());
            if gated && identity.principal().is_none() {
                debug!(
                    target: "albedo.auth",
                    route = %route_pattern,
                    "anonymous request refused by the route's `auth = \"required\"`"
                );
                if state.request_timings {
                    crate::timing::print_request(method.as_str(), &path, started.elapsed());
                }
                return crate::handlers::auth_gate::refuse_anonymous(&route_pattern);
            }
            // AUTH · the render path's identity — the third of the three
            // (render, action, subscribe) `AUTH.md` § 5 says resolve through one
            // place, and the one P1 consumes, because `user.id` in a component
            // body is read here. Resolved above, before the render rather than
            // inside it: a render that acquired its own principal partway through
            // could produce a page whose head was built for a stranger and whose
            // body was built for somebody, which is precisely the
            // cross-principal bleed invariant 2.2 forbids.
            let response = streaming_handler_with_match(
                streaming_runtime.clone(),
                request,
                route_pattern,
                params,
                identity,
            )
            .await
            .into_response();
            if state.request_timings {
                crate::timing::print_request(method.as_str(), &path, started.elapsed());
            }
            return response;
        }
    }

    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            return RuntimeError::RequestBodyRead(err.to_string()).into_response();
        }
    };

    let request_context = RequestContext::new(
        method,
        path.clone(),
        query.as_deref(),
        matched.params,
        &parts.headers,
        body,
    );

    // Phase-F: if `handler_id` resolves to an API handler,
    // dispatch through the API path. Otherwise fall through to
    // the page-route flow (middleware, auth, handler, layout).
    if let Some(api_handler) = world.api_handlers.get(&matched.target.handler_id).cloned() {
        return run_api_request(&world, matched.target, request_context, api_handler).await;
    }

    let mut request_context = request_context;
    let rendered = match execute_route(&world, matched.target, &mut request_context).await {
        Ok(response) => response.into_response(),
        Err(err) => {
            error!(request_id = request_context.request_id, error = %err, "request failed");
            err.into_response()
        }
    };
    if state.request_timings {
        crate::timing::print_request(method.as_str(), &path, started.elapsed());
    }
    rendered
}

/// SHUTTER · the class a matched route answers to.
///
/// 🔑 **Derived from the build, not authored and not guessed from the path** —
/// and the rule itself lives in
/// [`classify_route`](dom_render_compiler::shutter::classify_route) rather than
/// here, because `albedo doctor` prints the same derivation. Two copies would let
/// the audit artefact drift from the system it claims to describe, which is the
/// standing "three implementations of the paint rule" shape.
///
/// Non-streaming routes run a userland handler and are priced as reads — the
/// handler is opaque to us, and pricing "we cannot see inside this" as free would
/// make it the cheapest thing to abuse.
fn matched_route_class(
    world: &RenderWorld,
    matched: &crate::routing::MatchedRoute,
    method: HttpMethod,
    path: &str,
) -> OperationClass {
    if !should_use_manifest_streaming(world, &matched.target, method, path) {
        return OperationClass::Read;
    }
    let Some(streaming) = world.streaming_runtime.as_ref() else {
        return OperationClass::Read;
    };
    let pattern = matched.target.entry_module.as_deref().unwrap_or(path);
    streaming
        .manifest
        .routes
        .get(pattern)
        .map_or(OperationClass::StaticRead, |route| {
            dom_render_compiler::shutter::classify_route(route)
        })
}

/// HTTP header bakabox sets to carry the session id alongside each
/// action POST. Mirrors the WT-layer header used during session
/// handshake. Production deployments should bind a signed cookie at
/// session-open time and prefer that over the plain header.
const ACTION_SESSION_HEADER: &str = "x-albedo-session";

/// Phase-G/H — runs the action HTTP route. Reads the body, builds a
/// `RequestContext`, extracts a session id from the
/// `x-albedo-session` header (synthesising a random one when absent so
/// handlers never see `None`), and dispatches to [`run_action_request`]
/// with a [`SessionSlots`] view bound to the server's shared slot
/// store. The body cap matches `MAX_REQUEST_BODY_BYTES` so an oversized
/// envelope is rejected with the same shape as any other large request.
async fn run_action_route(
    world: &RenderWorld,
    dev_error_registry: Option<&crate::dev::SharedErrorRegistry>,
    request: Request<Body>,
    // AUTH · who is invoking this action. Carried in rather than resolved here
    // so the identity a request renders under and the one it acts under come
    // from the same call — two resolutions could disagree, and an action running
    // as a different principal than the page that offered it is the
    // confused-deputy shape.
    principal: crate::auth::Identity,
) -> Response {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => return RuntimeError::RequestBodyRead(err.to_string()).into_response(),
    };

    // Phase L · prefer the `__Host-albedo-session` cookie (set by the
    // streaming handler on first page render) over the explicit
    // `x-albedo-session` header. Browser-driven form POSTs auto-send
    // the cookie; programmatic clients can still override via the
    // header. Without either, fall back to a fresh random session —
    // which will trip CSRF validation on a subsequent submit, which
    // is the correct failure mode.
    let session_id = crate::render::csrf::read_session_cookie(&parts.headers)
        .or_else(|| {
            parts
                .headers
                .get(ACTION_SESSION_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
                .map(SessionId::new)
        })
        .unwrap_or_else(SessionId::random);

    let query = parts.uri.query().map(str::to_string);
    // AUTH F1 · the resolved principal rides the context to the write path.
    //
    // Carried on the value that already reaches `ActionHandler::handle` rather
    // than through a new parameter or a task-local: the context is the request's
    // "what is known here" object, the handler already receives it, and an
    // ambient identity is the shape a confused deputy takes. `principal` was
    // resolved by the caller, so this cannot disagree with the identity the page
    // was rendered under.
    let ctx = RequestContext::new(
        HttpMethod::Post,
        parts.uri.path().to_string(),
        query.as_deref(),
        Default::default(),
        &parts.headers,
        body.clone(),
    )
    .with_principal(principal.principal().map(|who| who.id.clone()));

    // AUTH · P0 records the identity on the action path; F1's write-path
    // enforcement is what consumes it. Logged at debug so a "why did this action
    // see nobody" question stays answerable from the outside.
    if let Some(who) = principal.principal() {
        tracing::debug!(
            target: "albedo.auth",
            principal = %who.id,
            provider = %who.provider,
            "action dispatched under a resolved principal"
        );
    }

    let slots = SessionSlots::new(session_id, world.slot_store.clone());
    run_action_request(
        world.action_handlers.as_ref(),
        world.csrf.as_ref(),
        world.form_action_ids.as_ref(),
        world.gated_action_ids.as_ref(),
        ctx,
        body,
        slots,
        dev_error_registry,
    )
    .await
}

/// The action name in `POST /_albedo/action/{name}`, or `None` for any other
/// path.
///
/// Matched here rather than by a router pattern because the answer has to be
/// *exactly* one segment: `/_albedo/action/a/b` is not an action called `a/b`,
/// and neither is `/_albedo/action/` an action called nothing. Both fall through
/// to the ordinary 404 rather than reaching the dispatcher with a name it would
/// then have to re-validate.
///
/// The alphabet is `transforms::form::is_url_safe_action_name` — the same
/// predicate the renderers used to decide whether to emit a URL at all — so a
/// name this accepts is a name a form could have been built from. A request
/// naming anything else did not come from a page we served.
fn form_action_segment(path: &str) -> Option<String> {
    use dom_render_compiler::transforms::form::{is_url_safe_action_name, ACTION_ENDPOINT_PREFIX};
    let name = path.strip_prefix(ACTION_ENDPOINT_PREFIX)?;
    if !is_url_safe_action_name(name) {
        return None;
    }
    Some(name.to_string())
}

/// `POST /_albedo/action/{name}` — the browser's own form submit.
///
/// Structurally identical to [`run_action_route`] and deliberately so: same
/// session resolution, same [`RequestContext`], same principal, same registry.
/// The action is named by the URL instead of by the body, and the answer is a
/// redirect instead of an opcode frame. Everything in between is the one
/// dispatcher.
///
/// Kept as a sibling rather than a branch inside `run_action_route` because the
/// two differ in their *first* step — one decodes a bincode envelope, one decodes
/// a form — and a function that begins with "which kind of request is this" is
/// where a gate ends up applying to one arm and not the other.
async fn run_form_action_route(
    world: &RenderWorld,
    dev_error_registry: Option<&crate::dev::SharedErrorRegistry>,
    request: Request<Body>,
    action_name: String,
    principal: crate::auth::Identity,
) -> Response {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => return RuntimeError::RequestBodyRead(err.to_string()).into_response(),
    };

    let session_id = crate::render::csrf::read_session_cookie(&parts.headers)
        .or_else(|| {
            parts
                .headers
                .get(ACTION_SESSION_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
                .map(SessionId::new)
        })
        .unwrap_or_else(SessionId::random);

    let query = parts.uri.query().map(str::to_string);
    let ctx = RequestContext::new(
        HttpMethod::Post,
        parts.uri.path().to_string(),
        query.as_deref(),
        Default::default(),
        &parts.headers,
        body.clone(),
    )
    .with_principal(principal.principal().map(|who| who.id.clone()));

    if let Some(who) = principal.principal() {
        tracing::debug!(
            target: "albedo.auth",
            principal = %who.id,
            provider = %who.provider,
            action = %action_name,
            "form action dispatched under a resolved principal"
        );
    }

    let slots = SessionSlots::new(session_id, world.slot_store.clone());
    crate::handlers::action::run_form_action_request(
        world.action_handlers.as_ref(),
        world.csrf.as_ref(),
        world.form_action_ids.as_ref(),
        world.gated_action_ids.as_ref(),
        ctx,
        &action_name,
        body,
        slots,
        dev_error_registry,
    )
    .await
}

/// Runs an API request: applies the route-level timeout, calls
/// [`dispatch_api_route`], and converts the result into an axum
/// response. Centralised so the dispatcher stays linear and so future
/// per-request observability (tracing, metrics) attaches in one place.
async fn run_api_request(
    world: &RenderWorld,
    target: RouteTarget,
    ctx: RequestContext,
    handler: SharedApiHandler,
) -> Response {
    let request_id = ctx.request_id.clone();
    let dispatch = dispatch_api_route(&target, ctx, &world.auth_provider, &handler);
    let result = tokio::time::timeout(world.request_timeout, dispatch).await;
    match result {
        Ok(Ok(api_response)) => api_response.into_response(),
        Ok(Err(err)) => {
            error!(request_id, error = %err, "api request failed");
            err.into_response()
        }
        Err(_) => {
            let err = RuntimeError::RequestHandling(format!(
                "api request timed out after {} ms",
                world.request_timeout.as_millis()
            ));
            error!(request_id, error = %err, "api request timed out");
            err.into_response()
        }
    }
}

async fn execute_route(
    world: &RenderWorld,
    target: RouteTarget,
    ctx: &mut RequestContext,
) -> Result<ResponsePayload, RuntimeError> {
    for middleware_id in &target.middleware {
        let middleware = world.middleware.get(middleware_id).ok_or_else(|| {
            RuntimeError::MiddlewareNotFound {
                middleware_id: middleware_id.clone(),
            }
        })?;
        middleware.on_request(ctx).await?;
    }

    if let Some(policy) = &target.auth {
        match world.auth_provider.authorize(ctx, policy).await? {
            AuthDecision::Allow => {}
            AuthDecision::Deny { reason } => {
                return Err(RuntimeError::Authentication(reason));
            }
        }
    }

    let handler = world
        .handlers
        .get(target.handler_id.as_str())
        .ok_or_else(|| RuntimeError::HandlerNotFound {
            handler_id: target.handler_id.clone(),
        })?
        .clone();

    let ctx_for_response_hooks = ctx.clone();
    let response_fut = handler.handle(ctx.clone());
    let mut response = tokio::time::timeout(world.request_timeout, response_fut)
        .await
        .map_err(|_| {
            RuntimeError::RequestHandling(format!(
                "request timed out after {} ms",
                world.request_timeout.as_millis()
            ))
        })??;

    if !target.layout_handlers.is_empty() {
        apply_layout_handlers(world, target.layout_handlers.as_slice(), ctx, &mut response).await?;
    }

    for middleware_id in target.middleware.iter().rev() {
        let middleware = world.middleware.get(middleware_id).ok_or_else(|| {
            RuntimeError::MiddlewareNotFound {
                middleware_id: middleware_id.clone(),
            }
        })?;
        middleware
            .on_response(&ctx_for_response_hooks, &mut response)
            .await?;
    }

    Ok(response)
}
/// A valid patches stream that will never carry anything — the answer when no
/// broadcast registry is wired, so the client's lane stays unconditional
/// instead of having a failure branch that only appears in some builds.
async fn empty_patch_stream() -> axum::response::Response<axum::body::Body> {
    crate::handlers::serve_patch_stream(
        Arc::new(dom_render_compiler::runtime::BroadcastRegistry::new()),
        &[],
        None,
    )
    .await
    .into_response()
}

/// The broadcast topics the page at `page_path` reads.
///
/// Resolves the concrete path to its manifest key exactly as the render path
/// does — router match, then `entry_module` as the pattern (`/essays/[slug]`)
/// — so a dynamic route's patches lane subscribes to the same topics its HTML
/// was rendered from. An unmatched path yields no topics rather than an error:
/// the client opens this lane unconditionally, and a page that turns out to
/// read nothing should get a quiet empty stream.
/// `None` when `page_path` matches no GET route at all — the caller decides
/// whether that means "no topics" (the per-tab lane, which serves an empty
/// stream) or "denied" (the PHOSPHOR subscribe path, which refuses to
/// subscribe a path that doesn't exist). A matched route with no topics is
/// `Some(vec![])` — subscribable, and legitimately quiet.
fn resolve_route_topics(
    world: &RenderWorld,
    streaming: &Arc<StreamingAppState>,
    page_path: &str,
    principal: Option<&dom_render_compiler::auth::PrincipalId>,
) -> Option<Vec<String>> {
    Some(resolve_route_topics_detailed(world, streaming, page_path, None, principal)?.0)
}

/// [`resolve_route_topics`], keeping the resolved partitions alongside the topic
/// list.
///
/// The topic strings are what the subscribe protocol wants; the
/// [`ResolvedPartition`]s are what the warmer wants (it needs the collection and
/// the key to run the query, and re-deriving those by splitting the topic string
/// back apart would be a second implementation of the naming rule — exactly the
/// drift invariant 5 forbids).
///
/// APERTURE · `sources_registry` is threaded in rather than reached through
/// `world` because the registry lives in the **persistent** tier beside the
/// FORGE schema, not in the swappable world — a dev hot reload must not re-mint
/// it and throw away a warm response cache. `None` means the app declared no
/// sources, in which case the manifest's source specs cannot resolve and the
/// list is empty.
fn resolve_route_topics_detailed(
    world: &RenderWorld,
    streaming: &Arc<StreamingAppState>,
    page_path: &str,
    sources_registry: Option<&dom_render_compiler::aperture::SourceRegistry>,
    // AUTH item 5 P1 · the *lane's* principal, not the tab's session. A lane
    // that is not signed in resolves no identity-keyed topic and is therefore
    // granted none, which is what makes "cannot name it" an enforcement rather
    // than a description.
    principal: Option<&dom_render_compiler::auth::PrincipalId>,
) -> Option<(Vec<String>, Vec<ResolvedPartition>, Vec<ResolvedSourceTopic>)> {
    let RouteMatch::Matched(matched) = world.router.match_route(HttpMethod::Get, page_path) else {
        return None;
    };
    let pattern = matched
        .target
        .entry_module
        .clone()
        .unwrap_or_else(|| page_path.to_string());
    let Some(route) = streaming.manifest.routes.get(pattern.as_str()) else {
        return Some((Vec::new(), Vec::new(), Vec::new()));
    };

    // AUTH § 8.1.3 · the route's declared gate, enforced on the live lanes.
    //
    // 🔑 **This is the second half of a check that shipped as one.** The gate
    // was built at the page render and nowhere else, on the reasoning recorded
    // at `RouteAuthority` — *a subscribe grants exactly the read the page GET
    // already granted*. That held while the only way a page GET could refuse
    // was resolving no topic. `export const auth` added a second way to refuse,
    // and it is not expressible as an absent topic: a route over global data
    // resolves the same public topics for everyone, so an anonymous lane was
    // handed the rows of a page whose GET had just answered 401.
    //
    // This is F2's shape a second time. The line never changed; what it was
    // reasoning about did.
    //
    // Placed **before** resolution rather than after, for the reason the render
    // path gives at its own gate: a refusal must cost nothing, and warming a
    // partition out of FORGE for a lane that may not have it is work done on
    // behalf of a request already refused.
    if !route.auth.allows_anonymous() && principal.is_none() {
        debug!(
            target: "albedo.auth",
            route = %pattern,
            "anonymous lane refused by the route's `auth = \"required\"`"
        );
        return None;
    }

    // PRISM · the same resolver the render path calls, over the same matched
    // params. A spec whose param the route did not match, or whose key is
    // outside the alphabet, contributes no topic — the page is live for
    // everything else it reads and static for this one.
    let partitions =
        resolve_partition_topics(&route.shared_slot_partitions, principal, |name| {
            matched.params.get(name).map(String::as_str)
        });

    // APERTURE · the other derivation, resolved from the same matched params by
    // the same rule. A source binding is reachable only through a route that
    // renders it — invariant 2 is not weakened by the topic being remote.
    let sources = sources_registry.map_or_else(Vec::new, |registry| {
        dom_render_compiler::runtime::resolve_source_topics(
            &route.shared_slot_sources,
            registry,
            |name| matched.params.get(name).map(String::as_str),
        )
    });

    let mut topics = route.shared_slot_topics.clone();
    topics.extend(partitions.iter().map(|resolved| resolved.topic.clone()));
    topics.extend(sources.iter().map(|resolved| resolved.topic.clone()));
    Some((topics, partitions, sources))
}

/// PHOSPHOR's [`crate::handlers::phosphor::RouteAuthority`] over the live
/// world: the same resolution the render and the per-tab lane use, plus the
/// deny-on-unmatched rule.
///
/// PRISM · this now grants **partitions** as well as compile-time topics, and
/// identity is still unused — deliberately, and it is not a regression. A
/// partition is reachable only through a route that renders it (invariant 2), so
/// the subscribe path grants exactly the read the page GET already granted.
/// Item 5 adds the `user.id` key source and a per-topic policy check inside this
/// same function; the protocol, the envelope, the election and the caps do not
/// move.
struct WorldRouteAuthority {
    world: Arc<RenderWorld>,
    projector: Option<Arc<dyn dom_render_compiler::forge::RowProjector>>,
    /// The persistent tier — schema + substrate + registry — so read-through
    /// materialisation survives a dev world swap. Pinned here rather than
    /// reached through `world` for the same reason the action adapters hold it:
    /// a rebuilt world has a fresh registry, and a partition warmed into the
    /// old one would be invisible to the new one.
    live: LiveRuntime,
}

#[async_trait::async_trait]
impl crate::handlers::phosphor::RouteAuthority for WorldRouteAuthority {
    /// Resolve, warm, then grant.
    ///
    /// The warm is inside the choke point rather than beside it because a topic
    /// this function returns is one the caller is about to snapshot under its
    /// lock. Granting a partition that has not been materialised would seed the
    /// joining tab with an empty room and leave it that way until somebody
    /// wrote — indistinguishable, from the browser, from a room that really is
    /// empty.
    async fn authorize_route(
        &self,
        principal: Option<&dom_render_compiler::auth::PrincipalId>,
        path: &str,
    ) -> Option<Vec<String>> {
        let streaming = self.world.streaming_runtime.as_ref()?;
        let registry = self
            .live
            .source_reader
            .get()
            .map(|reader| reader.registry().as_ref());
        let (topics, partitions, sources) =
            resolve_route_topics_detailed(&self.world, streaming, path, registry, principal)?;
        if !partitions.is_empty() {
            crate::topics::TopicWarmer::warm(&self.live, &partitions).await;
        }
        if !sources.is_empty() {
            crate::topics::TopicWarmer::warm_sources(&self.live, &sources).await;
        }
        Some(topics)
    }

    fn registry(&self) -> Arc<BroadcastRegistry> {
        self.world.broadcast.clone()
    }

    fn projector(&self) -> Option<Arc<dyn dom_render_compiler::forge::RowProjector>> {
        self.projector.clone()
    }
}

fn should_use_manifest_streaming(
    world: &RenderWorld,
    target: &RouteTarget,
    method: HttpMethod,
    path: &str,
) -> bool {
    if !matches!(method, HttpMethod::Get | HttpMethod::Head) {
        return false;
    }

    if target.entry_module.is_none() {
        return false;
    }

    if target.props_loader.is_some() || target.auth.is_some() {
        return false;
    }

    if !target.middleware.is_empty() || !target.layout_handlers.is_empty() {
        return false;
    }

    // The manifest is keyed by route pattern, not the concrete request path, so
    // a dynamic route (`/essays/[slug]`) would never match on the literal
    // `path` (`/essays/my-essay`). `entry_module` carries the manifest key (set
    // by `boot_production_server`); fall back to `path` for static routes whose
    // key and path coincide.
    let manifest_key = target.entry_module.as_deref().unwrap_or(path);

    world
        .streaming_runtime
        .as_ref()
        .map(|runtime| runtime.manifest.routes.contains_key(manifest_key))
        .unwrap_or(false)
}

async fn apply_layout_handlers(
    world: &RenderWorld,
    layout_handlers: &[String],
    ctx: &RequestContext,
    response: &mut ResponsePayload,
) -> Result<(), RuntimeError> {
    if !response_is_html(response) {
        return Ok(());
    }

    let mut wrapped_html = match &response.body {
        ResponseBody::Full(body) => std::str::from_utf8(body.as_ref())
            .map_err(|err| {
                RuntimeError::RequestHandling(format!("failed to decode HTML body: {err}"))
            })?
            .to_string(),
        ResponseBody::Stream(chunks) => {
            let mut combined = Vec::new();
            for chunk in chunks {
                combined.extend_from_slice(chunk.as_ref());
            }
            std::str::from_utf8(combined.as_slice())
                .map_err(|err| {
                    RuntimeError::RequestHandling(format!(
                        "failed to decode streamed HTML body: {err}"
                    ))
                })?
                .to_string()
        }
    };

    for layout_id in layout_handlers.iter().rev() {
        let layout = world
            .layouts
            .get(layout_id)
            .ok_or_else(|| RuntimeError::LayoutNotFound {
                layout_id: layout_id.clone(),
            })?;
        wrapped_html = layout.wrap(ctx.clone(), wrapped_html).await?;
    }

    response.body = ResponseBody::Full(wrapped_html.into_bytes().into());
    response.headers.insert(
        "content-type".to_string(),
        "text/html; charset=utf-8".to_string(),
    );
    Ok(())
}

fn response_is_html(response: &ResponsePayload) -> bool {
    response
        .headers
        .get("content-type")
        .map(|value| value.to_ascii_lowercase().starts_with("text/html"))
        .unwrap_or(false)
}

async fn shutdown_signal(_timeout: Duration) {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiResponse;
    use crate::config::{RouteSpec, ServerConfig};
    use crate::routing::{AuthPolicy, HttpMethod};
    use axum::body::to_bytes;
    use bytes::Bytes;
    use tower::ServiceExt;

    /// The no-JS action route matches exactly one segment, and only a segment a
    /// form could have been built from. Every rejection here would otherwise
    /// reach the dispatcher as a "name" it would have to re-validate — and the
    /// traversal cases are the reason `.` is outside the alphabet.
    #[test]
    fn the_form_action_route_matches_exactly_one_safe_segment() {
        assert_eq!(
            form_action_segment("/_albedo/action/sign_guestbook").as_deref(),
            Some("sign_guestbook")
        );
        for path in [
            "/_albedo/action",     // the envelope route, not this one
            "/_albedo/action/",    // an action called nothing
            "/_albedo/action/a/b", // not an action called `a/b`
            "/_albedo/action/..",
            "/_albedo/action/../../etc/passwd",
            "/_albedo/action/a%2Fb",
            "/_albedo/actions/x",
            "/mine",
        ] {
            assert_eq!(form_action_segment(path), None, "`{path}` must not match");
        }
    }

    #[tokio::test]
    async fn test_dynamic_route_dispatches_and_reads_param() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "users.show".to_string(),
                method: HttpMethod::Get,
                path: "/users/{id}".to_string(),
                handler: "users.show".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("users.show", |ctx: RequestContext| async move {
                let id = ctx.params.get("id").cloned().unwrap_or_default();
                Ok(ResponsePayload::ok_text(format!("user={id}")))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/users/42?include=profile")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "user=42");
    }

    /// A server whose only route is a plain handler, for the SHUTTER tests
    /// below. Every one of them shares a single limiter, because
    /// `AlbedoServer::router()` clones the same `RuntimeState`.
    fn rationed_server() -> AlbedoServer {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "ping".to_string(),
                method: HttpMethod::Get,
                path: "/ping".to_string(),
                handler: "ping".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };
        AlbedoServerBuilder::new(config)
            .register_handler("ping", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_text("pong"))
            })
            .build()
            .unwrap()
    }

    fn get(uri: &str, peer: Option<SocketAddr>) -> Request<Body> {
        let mut request = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        if let Some(peer) = peer {
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
        }
        request
    }

    /// SHUTTER · a client that only learns its budget once it has run out cannot
    /// pace itself, which is how a well-behaved integration becomes a thundering
    /// herd. The headers therefore ride **admitted** responses, which is a
    /// property of the dispatcher and not of the header helper.
    #[tokio::test]
    async fn an_admitted_response_carries_its_remaining_budget() {
        let server = rationed_server();
        let response = server.router().oneshot(get("/ping", None)).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().contains_key("ratelimit-remaining"),
            "an admitted response was not stamped: {:?}",
            response.headers()
        );
        assert!(response.headers().contains_key("ratelimit-limit"));
    }

    /// The limiter is actually on the request path. Not "the module compiles" —
    /// a flood through the real dispatcher must come back 429 with everything a
    /// client needs to back off correctly.
    #[tokio::test]
    async fn a_flood_is_refused_by_the_dispatcher_with_an_actionable_refusal() {
        let server = rationed_server();
        let peer: SocketAddr = "198.51.100.7:4444".parse().unwrap();

        let mut refusal = None;
        for _ in 0..1_000 {
            let response = server
                .router()
                .oneshot(get("/ping", Some(peer)))
                .await
                .unwrap();
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                refusal = Some(response);
                break;
            }
        }

        let refusal = refusal.expect("a thousand requests were all admitted — nothing is rationed");
        assert!(
            refusal.headers().contains_key(axum::http::header::RETRY_AFTER),
            "a refusal without Retry-After tells a client to guess"
        );
        let body = to_bytes(refusal.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("rate_limited"), "{body}");
        // The refusal explains its own derivation — the thing a path-and-IP
        // limiter cannot do, and the reason this one does not get disabled.
        assert!(body.contains("\"why\""), "{body}");
    }

    /// The peer address reaches the limiter. Without the connect-info plumbing
    /// this passes trivially and means nothing, so it asserts the *separation*:
    /// one address exhausting its budget must not refuse another's first request.
    #[tokio::test]
    async fn one_address_cannot_spend_another_addresss_budget() {
        let server = rationed_server();
        let attacker: SocketAddr = "203.0.113.9:5555".parse().unwrap();
        let bystander: SocketAddr = "198.51.100.7:6666".parse().unwrap();

        let mut exhausted = false;
        for _ in 0..1_000 {
            let response = server
                .router()
                .oneshot(get("/ping", Some(attacker)))
                .await
                .unwrap();
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                exhausted = true;
                break;
            }
        }
        assert!(exhausted, "the attacker was never refused");

        let response = server
            .router()
            .oneshot(get("/ping", Some(bystander)))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "one address's flood refused a different address — the peer never reached the limiter"
        );
    }

    /// **The differentiation, made observable.** The class a request answers to
    /// is derived from what the route does, so two paths through the same
    /// dispatcher are charged against different limits — and the `RateLimit-Policy`
    /// header says which. A path-and-IP limiter has one answer for both.
    #[tokio::test]
    async fn the_class_a_request_is_charged_is_derived_from_the_route() {
        let server = rationed_server();

        let handled = server.router().oneshot(get("/ping", None)).await.unwrap();
        let policy = handled.headers()["ratelimit-policy"].to_str().unwrap().to_string();
        assert!(
            policy.contains("class=read"),
            "a route running a userland handler should answer to the read limit: {policy}"
        );

        // An unmatched path is still charged — a 404 flood is a flood — but at
        // the static rate, because that is what it costs.
        let missing = server
            .router()
            .oneshot(get("/nothing-here", None))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let policy = missing.headers()["ratelimit-policy"].to_str().unwrap().to_string();
        assert!(
            policy.contains("class=static-read"),
            "an unmatched path should answer to the static limit: {policy}"
        );
    }

    /// § 2d's unclaimed regression test, folded into the PHOSPHOR work: the
    /// dev endpoints gate on registry presence, so a production server (dev
    /// mode off) must 404 every one of them — they stream stack traces and
    /// app HTML, which is dev-only material. The PHOSPHOR trunk, by
    /// contrast, is a production surface and must stay up; its `?dev=1`
    /// flag is inert in prod (no registries → nothing to merge).
    #[tokio::test]
    async fn dev_endpoints_404_in_production_but_the_phosphor_trunk_stays_up() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .with_dev_mode(false)
            .build()
            .unwrap();

        for path in [
            "/_albedo/dev/stream",
            "/_albedo/dev/errors",
            "/_albedo/dev/hmr",
            "/_albedo/dev/stream.js",
            "/_albedo/dev/overlay.js",
            "/_albedo/dev/hmr-apply.js",
        ] {
            let response = server
                .router()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "dev endpoint must not exist in production: {path}"
            );
        }

        let trunk = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_albedo/phosphor?dev=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            trunk.status(),
            StatusCode::OK,
            "the lane is a production surface; the dev flag is merely inert"
        );
    }

    #[tokio::test]
    async fn test_method_guard_returns_405_with_allow_header() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "users.show".to_string(),
                method: HttpMethod::Get,
                path: "/users/{id}".to_string(),
                handler: "users.show".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("users.show", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_text("ok"))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/users/42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = response
            .headers()
            .get("allow")
            .and_then(|value| value.to_str().ok());
        assert_eq!(allow, Some("GET"));
    }

    struct DenyAllAuth;

    #[async_trait::async_trait]
    impl AuthProvider for DenyAllAuth {
        async fn authorize(
            &self,
            _ctx: &RequestContext,
            _policy: &AuthPolicy,
        ) -> Result<AuthDecision, RuntimeError> {
            Ok(AuthDecision::Deny {
                reason: "blocked".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn test_auth_policy_blocks_request() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "private".to_string(),
                method: HttpMethod::Get,
                path: "/private".to_string(),
                handler: "private.handler".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: Some(AuthPolicy::Required),
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("private.handler", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_text("secret"))
            })
            .with_auth_provider(DenyAllAuth)
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/private")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_nested_layout_handlers_wrap_html_in_order() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: vec![
                crate::config::LayoutSpec {
                    name: "root".to_string(),
                    path: "/".to_string(),
                    handler: "layout.root".to_string(),
                },
                crate::config::LayoutSpec {
                    name: "dashboard".to_string(),
                    path: "/dashboard".to_string(),
                    handler: "layout.dashboard".to_string(),
                },
            ],
            routes: vec![RouteSpec {
                name: "dashboard.home".to_string(),
                method: HttpMethod::Get,
                path: "/dashboard".to_string(),
                handler: "dashboard.page".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("dashboard.page", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_html("<main>Dashboard</main>"))
            })
            .register_layout(
                "layout.root",
                |_ctx: RequestContext, inner: String| async move {
                    Ok(format!("<html><body>{inner}</body></html>"))
                },
            )
            .register_layout(
                "layout.dashboard",
                |_ctx: RequestContext, inner: String| async move {
                    Ok(format!("<section class=\"dashboard\">{inner}</section>"))
                },
            )
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(
            body,
            "<html><body><section class=\"dashboard\"><main>Dashboard</main></section></body></html>"
        );
    }

    #[tokio::test]
    async fn test_streaming_html_response_chunks_are_emitted() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "stream.page".to_string(),
                method: HttpMethod::Get,
                path: "/stream".to_string(),
                handler: "stream.page".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("stream.page", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_html_stream([
                    Bytes::from_static(b"<main>"),
                    Bytes::from_static(b"ALBEDO"),
                    Bytes::from_static(b"</main>"),
                ]))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok());
        assert_eq!(content_type, Some("text/html; charset=utf-8"));
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "<main>ALBEDO</main>");
    }

    // ── Phase F — API route tests ─────────────────────────────────────

    fn api_route(
        method: HttpMethod,
        path: &str,
        handler: &str,
        auth: Option<AuthPolicy>,
    ) -> RouteSpec {
        RouteSpec {
            name: handler.to_string(),
            method,
            path: path.to_string(),
            handler: handler.to_string(),
            entry_module: None,
            props_loader: None,
            middleware: Vec::new(),
            auth,
        }
    }

    #[tokio::test]
    async fn api_handler_echoes_request_body() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(HttpMethod::Post, "/api/echo", "echo", None)],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("echo", |ctx: RequestContext| async move {
                Ok(ApiResponse::ok(ctx.body)
                    .with_header("content-type", "application/octet-stream"))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/echo")
                    .body(Body::from("hello-api"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/octet-stream")
        );
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "hello-api");
    }

    #[tokio::test]
    async fn api_handler_returns_json_with_correct_content_type() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(HttpMethod::Get, "/api/status", "status", None)],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("status", |_ctx: RequestContext| async move {
                ApiResponse::json(&serde_json::json!({ "ok": true, "version": 1 }))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, r#"{"ok":true,"version":1}"#);
    }

    #[tokio::test]
    async fn api_handler_with_required_auth_returns_401_when_denied() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(
                HttpMethod::Get,
                "/api/private",
                "private",
                Some(AuthPolicy::Required),
            )],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("private", |_ctx: RequestContext| async move {
                Ok(ApiResponse::ok(Bytes::from_static(b"secret")))
            })
            .with_auth_provider(DenyAllAuth)
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/private")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "denied auth must surface as 401 on the API path"
        );
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert!(
            !body.as_ref().eq(b"secret"),
            "handler body must never reach the wire when auth denies"
        );
    }

    #[tokio::test]
    async fn api_handler_with_role_auth_runs_when_provider_allows() {
        // Mirrors the Phase-F risk-#9 mitigation test: an admin-only
        // route must invoke the handler when the auth provider says yes.
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(
                HttpMethod::Get,
                "/api/admin",
                "admin",
                Some(AuthPolicy::Role("admin".to_string())),
            )],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("admin", |_ctx: RequestContext| async move {
                Ok(ApiResponse::ok(Bytes::from_static(b"admin-area")))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "admin-area");
    }

    #[tokio::test]
    async fn api_handler_method_mismatch_returns_405() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(HttpMethod::Get, "/api/users", "users.list", None)],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("users.list", |_ctx: RequestContext| async move {
                Ok(ApiResponse::ok(Bytes::from_static(b"[]")))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = response
            .headers()
            .get("allow")
            .and_then(|v| v.to_str().ok());
        assert_eq!(allow, Some("GET"));
    }

    // ── Phase G — action route tests ──────────────────────────────────

    /// Mint the per-session CSRF token the action gate now requires on
    /// EVERY POST (see `handlers::action`). Tests that pin
    /// `x-albedo-session` to `session_uuid` present the returned token in
    /// the `x-albedo-csrf` header — the server-side mirror of the browser
    /// reading the shell's `__ALBEDO_CSRF__` global.
    fn action_csrf_token(server: &AlbedoServer, session_uuid: &str) -> String {
        let session = dom_render_compiler::runtime::SessionId::new(
            uuid::Uuid::parse_str(session_uuid).expect("valid session uuid"),
        );
        server.csrf_registry().token_for(session)
    }

    /// **The two-part payment, through the real dispatcher.** An action's blast
    /// radius is not knowable at admission, so it is settled afterwards — and the
    /// place that has to be true is the composition, not the pieces. This drives
    /// the actual `/_albedo/action` branch and asserts that a dispatch which
    /// reported fan-out leaves *less* budget behind than one that reported none.
    ///
    /// Note which response carries the drop: not the noisy one. Its own headers
    /// are stamped from the admission verdict, because at that instant nobody
    /// knew what it would cost. The next request is the one that pays, which is
    /// the honest shape — a limiter does not un-commit a write.
    #[tokio::test]
    async fn a_dispatch_that_reached_many_lanes_costs_the_next_one_more() {
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::{Instruction, StableId, TagId};

        const QUIET: u32 = 7;
        const NOISY: u32 = 8;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_action(QUIET, |_ctx, envelope: ActionEnvelope, _slots| async move {
                Ok(vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(envelope.action_id),
                }])
            })
            .register_action(NOISY, |_ctx, envelope: ActionEnvelope, _slots| async move {
                // Stands in for what `apply_writes` reports back from inside its
                // transaction: this write is about to reach 512 open lanes.
                crate::shutter::note_fan_out(512);
                Ok(vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(envelope.action_id),
                }])
            })
            .build()
            .unwrap();

        async fn dispatch(server: &AlbedoServer, action_id: u32, peer: SocketAddr) -> u32 {
            let body = encode_action_envelope(&ActionEnvelope {
                action_id,
                event_kind: 0,
                payload: Vec::new(),
            })
            .unwrap();
            let session_uuid = uuid::Uuid::new_v4().to_string();
            let token = action_csrf_token(server, &session_uuid);
            let mut request = Request::builder()
                .method("POST")
                .uri("/_albedo/action")
                .header("x-albedo-session", session_uuid.as_str())
                .header("x-albedo-csrf", token.as_str())
                .body(Body::from(body))
                .unwrap();
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));

            let response = server.router().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.headers()["ratelimit-remaining"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap()
        }

        // Two addresses so the two histories cannot contaminate each other.
        let calm: SocketAddr = "198.51.100.7:1111".parse().unwrap();
        let busy: SocketAddr = "203.0.113.9:2222".parse().unwrap();

        dispatch(&server, QUIET, calm).await;
        let after_quiet = dispatch(&server, QUIET, calm).await;

        dispatch(&server, NOISY, busy).await;
        let after_noisy = dispatch(&server, QUIET, busy).await;

        assert!(
            after_noisy < after_quiet,
            "a write that reached 512 lanes cost the same as one that reached none \
             ({after_noisy} vs {after_quiet} remaining) — the surcharge never landed"
        );
    }

    #[tokio::test]
    async fn action_route_dispatches_and_returns_wire_encoded_opcode_frame() {
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::{Instruction, StableId, TagId};
        use dom_render_compiler::ir::wire::decode_frame;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };

        let server = AlbedoServerBuilder::new(config)
            .register_action(
                42,
                |_ctx: RequestContext,
                 envelope: dom_render_compiler::ir::action::ActionEnvelope,
                 _slots: SessionSlots| async move {
                    // Handler returns one Create that targets the action_id
                    // as its stable_id so the test can verify the args
                    // reached the handler unmodified.
                    Ok(vec![Instruction::Create {
                        tag_id: TagId(0),
                        stable_id: StableId(envelope.action_id),
                    }])
                },
            )
            .build()
            .unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 42,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();

        // The gate requires a token on every action; pin a session and
        // present its token like the browser runtime does.
        let session_uuid = uuid::Uuid::new_v4().to_string();
        let token = action_csrf_token(&server, &session_uuid);
        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .header("x-albedo-csrf", token.as_str())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = decode_frame(&bytes).expect("response decodes as OpcodeFrame");
        assert!(matches!(
            frame.instructions[0],
            Instruction::Create {
                stable_id: StableId(42),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn action_route_returns_404_for_unregistered_action_id() {
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config).build().unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 99,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();

        // Present a valid token so the request clears the CSRF gate and
        // reaches the handler lookup — the 404 under test is about the
        // unknown action_id, not a missing token.
        let session_uuid = uuid::Uuid::new_v4().to_string();
        let token = action_csrf_token(&server, &session_uuid);
        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .header("x-albedo-csrf", token.as_str())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn action_route_carries_request_context_to_handler() {
        // Verifies the handler sees the headers from the originating
        // request — Phase H / I will lean on this for CSRF tokens and
        // session-bearing cookies.
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_action(
                7,
                |ctx: RequestContext,
                 _env: dom_render_compiler::ir::action::ActionEnvelope,
                 _slots: SessionSlots| async move {
                    // Echo the token header back via SetText so the test
                    // can read it from the decoded response.
                    let token = ctx
                        .headers
                        .get("x-albedo-session")
                        .cloned()
                        .unwrap_or_default();
                    Ok(vec![
                        dom_render_compiler::ir::opcode::Instruction::SetText {
                            stable_id: dom_render_compiler::ir::opcode::StableId(1),
                            text: token.into_bytes(),
                        },
                    ])
                },
            )
            .build()
            .unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 7,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();

        // A real UUID session so the minted token matches (the gate now
        // validates the token against the resolved session). The handler
        // echoes the raw `x-albedo-session` header, so we assert against
        // that same value.
        let session_uuid = uuid::Uuid::new_v4().to_string();
        let token = action_csrf_token(&server, &session_uuid);
        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .header("x-albedo-csrf", token.as_str())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = dom_render_compiler::ir::wire::decode_frame(&bytes).unwrap();
        match &frame.instructions[0] {
            dom_render_compiler::ir::opcode::Instruction::SetText { text, .. } => {
                assert_eq!(text.as_slice(), session_uuid.as_bytes());
            }
            other => panic!("expected SetText, got {other:?}"),
        }
    }

    // ── Phase H — reactive slot store integration ─────────────────────

    #[tokio::test]
    async fn slot_state_persists_across_two_action_invocations_in_the_same_session() {
        // The Phase-H closing loop: action A writes a slot, action B
        // reads the same slot for the same session and gets the value
        // back. Distinct sessions stay isolated.
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::SlotId;
        use dom_render_compiler::ir::wire::decode_frame;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };

        // action_id 1 — writer: stores the payload bytes into slot 7.
        // action_id 2 — reader: emits a `SetText` carrying whatever's
        // currently in slot 7. Empty body when the slot is unset.
        let server = AlbedoServerBuilder::new(config)
            .register_action(
                1,
                |_ctx: RequestContext, env: ActionEnvelope, slots: SessionSlots| async move {
                    slots.write(SlotId(7), env.payload.clone());
                    Ok(Vec::new())
                },
            )
            .register_action(
                2,
                |_ctx: RequestContext, _env: ActionEnvelope, slots: SessionSlots| async move {
                    let current = slots.read(SlotId(7)).unwrap_or_default();
                    Ok(vec![
                        dom_render_compiler::ir::opcode::Instruction::SetText {
                            stable_id: dom_render_compiler::ir::opcode::StableId(1),
                            text: current,
                        },
                    ])
                },
            )
            .build()
            .unwrap();

        let session_uuid = uuid::Uuid::new_v4().to_string();
        // One token for the session, presented on both POSTs (stable per
        // session), so each clears the gate.
        let token = action_csrf_token(&server, &session_uuid);
        let router = server.router();

        // First POST — action 1 writes "hello-world" into slot 7.
        let write_body = encode_action_envelope(&ActionEnvelope {
            action_id: 1,
            event_kind: 0,
            payload: b"hello-world".to_vec(),
        })
        .unwrap();
        let write_response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .header("x-albedo-csrf", token.as_str())
                    .body(Body::from(write_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(write_response.status(), StatusCode::OK);
        // The write itself produced a SlotSet via the dirty drain.
        let write_bytes = to_bytes(write_response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (write_frame, _) = decode_frame(&write_bytes).unwrap();
        assert!(write_frame.instructions.iter().any(|instr| matches!(
            instr,
            dom_render_compiler::ir::opcode::Instruction::SlotSet { slot_id: SlotId(7), value }
                if value == b"hello-world"
        )));

        // Second POST — action 2 reads slot 7 for the same session and
        // emits the value back as the SetText payload.
        let read_body = encode_action_envelope(&ActionEnvelope {
            action_id: 2,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();
        let read_response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .header("x-albedo-csrf", token.as_str())
                    .body(Body::from(read_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(read_response.status(), StatusCode::OK);
        let read_bytes = to_bytes(read_response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (read_frame, _) = decode_frame(&read_bytes).unwrap();
        match &read_frame.instructions[0] {
            dom_render_compiler::ir::opcode::Instruction::SetText { text, .. } => {
                assert_eq!(
                    text.as_slice(),
                    b"hello-world",
                    "slot state must survive across action invocations within a session"
                );
            }
            other => panic!("expected SetText, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn slot_state_is_isolated_across_distinct_sessions() {
        // Same reader action, two different session ids → reads return
        // independent (empty) state.
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::SlotId;
        use dom_render_compiler::ir::wire::decode_frame;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_action(
                1,
                |_ctx: RequestContext, env: ActionEnvelope, slots: SessionSlots| async move {
                    slots.write(SlotId(7), env.payload.clone());
                    Ok(Vec::new())
                },
            )
            .register_action(
                2,
                |_ctx: RequestContext, _env: ActionEnvelope, slots: SessionSlots| async move {
                    let current = slots.read(SlotId(7)).unwrap_or_default();
                    Ok(vec![
                        dom_render_compiler::ir::opcode::Instruction::SetText {
                            stable_id: dom_render_compiler::ir::opcode::StableId(1),
                            text: current,
                        },
                    ])
                },
            )
            .build()
            .unwrap();

        let router = server.router();
        let session_a = uuid::Uuid::new_v4().to_string();
        let session_b = uuid::Uuid::new_v4().to_string();
        let token_a = action_csrf_token(&server, &session_a);
        let token_b = action_csrf_token(&server, &session_b);

        // Write under session A.
        let write_body = encode_action_envelope(&ActionEnvelope {
            action_id: 1,
            event_kind: 0,
            payload: b"a-only".to_vec(),
        })
        .unwrap();
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_a.as_str())
                    .header("x-albedo-csrf", token_a.as_str())
                    .body(Body::from(write_body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Read under session B — must NOT see session A's value.
        let read_body = encode_action_envelope(&ActionEnvelope {
            action_id: 2,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_b.as_str())
                    .header("x-albedo-csrf", token_b.as_str())
                    .body(Body::from(read_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = decode_frame(&bytes).unwrap();
        match &frame.instructions[0] {
            dom_render_compiler::ir::opcode::Instruction::SetText { text, .. } => {
                assert!(
                    text.is_empty(),
                    "session B must not see session A's slot value; got {:?}",
                    String::from_utf8_lossy(text)
                );
            }
            other => panic!("expected SetText, got {other:?}"),
        }
    }

    // ── Phase I — Navigate opcode + register_form_action ─────────────

    #[tokio::test]
    async fn action_handler_can_emit_navigate_opcode() {
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::Instruction;
        use dom_render_compiler::ir::wire::decode_frame;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_action(
                1,
                |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                    Ok(vec![Instruction::Navigate {
                        url: "/dashboard".to_string(),
                    }])
                },
            )
            .build()
            .unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 1,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();

        let session_uuid = uuid::Uuid::new_v4().to_string();
        let token = action_csrf_token(&server, &session_uuid);
        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .header("x-albedo-csrf", token.as_str())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = decode_frame(&bytes).unwrap();
        assert!(
            matches!(
                &frame.instructions[0],
                Instruction::Navigate { url } if url == "/dashboard"
            ),
            "Phase-I Navigate must round-trip through the action response wire path"
        );
    }

    /// A failed island SSR must reach the AUTHOR, not a log nobody subscribes to.
    ///
    /// 🪤 This project has paid for the distinction twice. `install_tracing`
    /// only installs a subscriber when `RUST_LOG` is set, so a `tracing::error!`
    /// on the island path is invisible on every ordinary run. A `<Link>` inside
    /// an island once removed an entire navigation bar from a real site with a
    /// green build and a clean console; the fix at the time raised `warn!` to
    /// `error!` — the same unheard channel — and a Radix dialog hit the
    /// identical silence months later. The level was never the problem.
    ///
    /// So the property under test is not "it is logged". It is that
    /// [`BootReport::lines`] — the one path the CLI prints — carries it, names
    /// the module, and repeats the renderer's own message verbatim.
    #[test]
    fn a_failed_island_render_reaches_the_boot_report() {
        let report = BootReport {
            island_ssr_failures: vec![crate::renderer_runtime::IslandRenderFailure {
                module_path: "src/components/DialogDemo.tsx".to_string(),
                error: "`DialogTrigger` must be used within `Dialog`".to_string(),
            }],
            ..BootReport::default()
        };

        let lines = report.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(line.contains("src/components/DialogDemo.tsx"), "{line}");
        // The component's OWN message is the useful half; summarising it away
        // would leave an author knowing only that something failed.
        assert!(line.contains("must be used within"), "{line}");
        // Phrased as absence. "failed to render" reads like a degraded page;
        // the component is simply not on it.
        assert!(line.contains("MISSING"), "{line}");

        assert!(
            BootReport::default().lines().is_empty(),
            "a clean boot must stay silent"
        );
    }

    /// The same property for the tier below, where the silence was worse.
    ///
    /// 🪤 A failed **Tier-A** render did not merely go unreported. The manifest
    /// builder fell back to scraping the component's own `.tsx` for the text
    /// between `<` and `>`, so a route rendered
    /// `<section data-albedo-static="SlotRoute">asChild );}</section>` — every
    /// tag stripped and the closing `);}` of the source file served to the
    /// browser — under a green build and a clean console. The scrape is gone;
    /// this asserts that what replaced it reaches the one path the CLI prints.
    ///
    /// The wording carries the part nobody guesses: a Tier-A render is a single
    /// call over the whole subtree, so a failing leaf deletes its **ancestors'**
    /// markup too. The page does not look degraded around the missing
    /// component — the section it lived in is gone.
    #[test]
    fn a_failed_tier_a_render_reaches_the_boot_report() {
        use dom_render_compiler::manifest::schema::StaticRenderFailure;

        let report = BootReport {
            static_render_failures: vec![StaticRenderFailure {
                kind: dom_render_compiler::manifest::schema::RenderFailureKind::StaticRender,
                component: "SlotDemo".to_string(),
                module_path: "src/components/SlotDemo.tsx".to_string(),
                error: "could not resolve import '@radix-ui/react-slot' from \
                        'components/SlotDemo.tsx'"
                    .to_string(),
            }],
            ..BootReport::default()
        };

        let lines = report.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(line.contains("SlotDemo"), "{line}");
        assert!(line.contains("src/components/SlotDemo.tsx"), "{line}");
        // The evaluator's own message names the exact specifier. Summarising it
        // leaves an author knowing only that something did not render.
        assert!(line.contains("@radix-ui/react-slot"), "{line}");
        assert!(line.contains("MISSING"), "{line}");
        // The ancestors are the surprise, so they are said out loud.
        assert!(line.contains("nests it"), "{line}");
    }

    #[tokio::test]
    async fn register_form_action_deserialises_json_payload_into_typed_struct() {
        use dom_render_compiler::ir::opcode::{Instruction, StableId};
        use dom_render_compiler::ir::wire::decode_frame;
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct LoginForm {
            username: String,
            password: String,
        }

        // Phase L · `register_form_action` now takes the action
        // name; the builder derives the wire-level `action_id` via
        // FNV-1a-32. The envelope below uses the same hash so the
        // dispatcher routes the request to the registered handler.
        const ACTION_NAME: &str = "submit_login";

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_form_action::<LoginForm, _, _>(
                ACTION_NAME,
                |_ctx: RequestContext, form: LoginForm, _slots: SessionSlots| async move {
                    // Echo the username back so the test can verify the
                    // typed payload made it through unchanged.
                    Ok(vec![
                        Instruction::SetText {
                            stable_id: StableId(1),
                            text: form.username.into_bytes(),
                        },
                        Instruction::Navigate {
                            url: format!("/welcome?ack={}", form.password.len()),
                        },
                    ])
                },
            )
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(signed_form_submit(
                &server,
                ACTION_NAME,
                serde_json::json!({ "username": "alice", "password": "hunter2" }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = decode_frame(&bytes).unwrap();
        match &frame.instructions[0] {
            Instruction::SetText { text, .. } => {
                assert_eq!(text.as_slice(), b"alice");
            }
            other => panic!("expected SetText, got {other:?}"),
        }
        match &frame.instructions[1] {
            Instruction::Navigate { url } => {
                assert_eq!(url, "/welcome?ack=7");
            }
            other => panic!("expected Navigate, got {other:?}"),
        }
    }

    /// Builds the action POST a browser would send for
    /// `<form action="action:NAME">`: the envelope the client encodes,
    /// plus the two things that get the request past the CSRF gate —
    /// the `__Host-albedo-session` cookie and the matching `_csrf` field in the
    /// form payload.
    ///
    /// Form actions fail closed without a token, so a test that means to
    /// exercise anything *downstream* of the gate has to present one.
    /// Minting it from the server's own registry (rather than stubbing
    /// the check) keeps these tests honest about the real request shape.
    fn signed_form_submit(
        server: &AlbedoServer,
        action_name: &str,
        payload: serde_json::Value,
    ) -> Request<Body> {
        use crate::render::form_action::form_action_id;
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};

        let session_uuid = uuid::Uuid::new_v4();
        let token = server
            .csrf_registry()
            .token_for(SessionId::new(session_uuid));

        let mut object = payload;
        object[crate::render::csrf::CSRF_FIELD_NAME] = serde_json::Value::String(token);

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: form_action_id(action_name),
            event_kind: 2, // Submit
            payload: serde_json::to_vec(&object).expect("payload encodes"),
        })
        .unwrap();

        Request::builder()
            .method("POST")
            .uri("/_albedo/action")
            .header(
                axum::http::header::COOKIE,
                format!(
                    "{}={session_uuid}",
                    crate::render::csrf::ALBEDO_SESSION_COOKIE
                ),
            )
            .body(Body::from(body))
            .unwrap()
    }

    /// A payload that clears the CSRF gate but cannot deserialize into
    /// the handler's declared type must surface as a 500 from the typed
    /// adapter.
    ///
    /// The payload is a well-formed JSON object missing a required field
    /// — not the raw `b"not json"` this test used to send. Since the gate
    /// began failing closed, non-JSON bytes can never reach a form
    /// action's handler at all (they can't carry a token), so a garbage
    /// payload would now assert the gate's 403 and quietly stop testing
    /// the decode path it was written for.
    #[tokio::test]
    async fn register_form_action_rejects_mismatched_payload_with_500() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct Required {
            #[allow(dead_code)]
            field: String,
        }

        const ACTION_NAME: &str = "malformed_required";

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_form_action::<Required, _, _>(
                ACTION_NAME,
                |_ctx: RequestContext, _form: Required, _slots: SessionSlots| async move {
                    panic!("handler must not run when payload fails to deserialize");
                },
            )
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(signed_form_submit(
                &server,
                ACTION_NAME,
                serde_json::json!({ "wrong_field": "value" }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// The gate, exercised through the whole server rather than the
    /// dispatcher in isolation: a real `register_form_action` submit
    /// with no token must be refused before the handler runs.
    ///
    /// This is the end-to-end statement of the bug — a Tier-B-rendered
    /// form used to send exactly this request (no CSRF input existed in
    /// its markup to serialize) and the server dispatched it.
    #[tokio::test]
    async fn form_action_submitted_without_a_token_is_refused_by_the_server() {
        use crate::render::form_action::form_action_id;
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};

        const ACTION_NAME: &str = "unsigned_submit";

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_form_action::<serde_json::Value, _, _>(
                ACTION_NAME,
                |_ctx: RequestContext, _form: serde_json::Value, _slots: SessionSlots| async move {
                    panic!("handler must not run for an unsigned form submit");
                },
            )
            .build()
            .unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: form_action_id(ACTION_NAME),
            event_kind: 2,
            payload: serde_json::to_vec(&serde_json::json!({ "user": "alice" })).unwrap(),
        })
        .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn missing_api_handler_id_fails_build() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(HttpMethod::Get, "/api/missing", "missing", None)],
        };

        // No api_handler registered for "missing" — build must reject.
        // `unwrap_err` would require AlbedoServer: Debug, so match by hand.
        match AlbedoServerBuilder::new(config).build() {
            Err(RuntimeError::HandlerNotFound { handler_id }) => {
                assert_eq!(handler_id, "missing");
            }
            Err(other) => panic!("expected HandlerNotFound, got {other:?}"),
            Ok(_) => panic!("build must reject a route with no registered handler"),
        }
    }
}

/// APERTURE A3 · the durable identity of one workflow, and whether a retry may
/// pick it up.
///
/// See [`workflow_identity`] for why the second field exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowIdentity {
    /// The id every derived idempotency key is built from (`{workflow}:{step}`).
    pub id: String,
    /// Whether a persisted log under this id may be resumed. False for an id
    /// this process invented, which nothing else can ever name.
    pub resumable: bool,
}

/// The form field carrying the client's intent token.
///
/// Reserved-prefixed like `_csrf` and `_albedo_return`, and stamped by the same
/// machinery — see `transforms::form`.
pub(crate) const INTENT_FIELD: &str = "_albedo_intent";

/// Derive a workflow's durable identity from the request.
///
/// # Why this cannot be derived from the payload alone
///
/// A retry must be recognised as the same intention; two deliberate clicks must
/// not be. Those two requests are **byte-identical** — same action, same fields
/// — so nothing in the envelope distinguishes them. Only the client knows which
/// it is sending, which is why every serious API that offers idempotency
/// (Stripe's among them) takes the key from the caller rather than computing it.
///
/// So the token comes from the client, and albedo mints it without the author
/// doing anything: the renderer stamps `_albedo_intent` into an action form the
/// way it already stamps `_csrf`, and the client runtime reuses it across its
/// own network retries. A no-JS resubmit (`F5` → *resend?*) replays the same
/// hidden field and resumes; a second deliberate submit comes from a fresh page
/// render with a fresh token and starts its own workflow. Both are correct, and
/// neither asked the author for anything.
///
/// # 🔑 Why the principal is in the id
///
/// The token is **client-supplied**, and the id it produces indexes a store of
/// *upstream response values*. Without scoping, a caller who guessed or
/// observed another user's token could resume their workflow and be handed
/// their responses on replay. Composing the id from `(action, principal,
/// intent)` — as separate hashed segments rather than one hash over the
/// concatenation — means a cross-principal collision needs two independent
/// collisions rather than one.
///
/// Anonymous requests share the `anon` segment, which is correct: they share a
/// principal in the only sense the server has.
pub(crate) fn workflow_identity(
    action_id: u32,
    principal: Option<&dom_render_compiler::auth::PrincipalId>,
    envelope: &dom_render_compiler::ir::action::ActionEnvelope,
) -> WorkflowIdentity {
    use dom_render_compiler::runtime::engine::stable_source_hash;

    let intent = serde_json::from_slice::<serde_json::Value>(&envelope.payload)
        .ok()
        .and_then(|payload| {
            payload
                .get(INTENT_FIELD)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .filter(|token| !token.trim().is_empty());

    match intent {
        Some(token) => {
            let who = principal.map_or_else(|| "anon".to_string(), ToString::to_string);
            WorkflowIdentity {
                id: format!(
                    "w_{action_id:08x}_{:016x}_{:016x}",
                    stable_source_hash(&who),
                    stable_source_hash(&token)
                ),
                resumable: true,
            }
        }
        // No token: today's behaviour exactly. A uuid is not resumable and must
        // not be — an id this process invented is one no retry can name, so
        // treating it as resumable would only ever produce a false miss.
        None => WorkflowIdentity {
            id: format!("w_{}", uuid::Uuid::new_v4().simple()),
            resumable: false,
        },
    }
}

/// APERTURE A3 · the identity that makes a persisted log findable.
#[cfg(test)]
mod workflow_identity_tests {
    use super::{workflow_identity, INTENT_FIELD};
    use dom_render_compiler::auth::PrincipalId;
    use dom_render_compiler::ir::action::ActionEnvelope;

    fn envelope(payload: serde_json::Value) -> ActionEnvelope {
        ActionEnvelope {
            action_id: 7,
            event_kind: 3,
            payload: serde_json::to_vec(&payload).expect("payload"),
        }
    }

    fn principal(raw: &str) -> PrincipalId {
        PrincipalId::parse(raw).expect("a valid principal")
    }

    /// The whole point: the same intention resolves to the same log.
    #[test]
    fn the_same_intent_token_names_the_same_workflow() {
        let user = principal("u_alice");
        let first = workflow_identity(7, Some(&user), &envelope(serde_json::json!({INTENT_FIELD: "t1"})));
        let again = workflow_identity(7, Some(&user), &envelope(serde_json::json!({INTENT_FIELD: "t1"})));
        assert_eq!(first.id, again.id);
        assert!(first.resumable);
    }

    /// And two intentions do not. A second deliberate submit arrives from a
    /// fresh render with a fresh token, and must start its own workflow rather
    /// than dedupe against the first.
    #[test]
    fn two_intent_tokens_name_two_workflows() {
        let user = principal("u_alice");
        let first = workflow_identity(7, Some(&user), &envelope(serde_json::json!({INTENT_FIELD: "t1"})));
        let second = workflow_identity(7, Some(&user), &envelope(serde_json::json!({INTENT_FIELD: "t2"})));
        assert_ne!(first.id, second.id);
    }

    /// 🔑 **The security property.** The token is client-supplied and the id it
    /// produces indexes a store of upstream *response values*. Without the
    /// principal in the id, a caller who guessed or observed another user's
    /// token could resume their workflow and be handed their responses.
    #[test]
    fn the_same_token_from_two_principals_names_two_workflows() {
        let payload = envelope(serde_json::json!({INTENT_FIELD: "stolen"}));
        let alice = principal("u_alice");
        let bob = principal("u_bob");
        assert_ne!(
            workflow_identity(7, Some(&alice), &payload).id,
            workflow_identity(7, Some(&bob), &payload).id,
            "🔴 one caller could resume another's workflow and read their responses"
        );
        assert_ne!(
            workflow_identity(7, Some(&alice), &payload).id,
            workflow_identity(7, None, &payload).id,
            "and anonymous is its own principal, not everyone's"
        );
    }

    /// Two different actions are two different workflows even under one token —
    /// a page's forms share a render and could share a stamp.
    #[test]
    fn two_actions_name_two_workflows_under_one_token() {
        let user = principal("u_alice");
        let payload = envelope(serde_json::json!({INTENT_FIELD: "t1"}));
        assert_ne!(
            workflow_identity(7, Some(&user), &payload).id,
            workflow_identity(8, Some(&user), &payload).id
        );
    }

    /// No token: today's behaviour exactly, and **not resumable**. An id this
    /// process invented is one no retry can name, so marking it resumable would
    /// only ever produce a wasted lookup.
    #[test]
    fn a_request_with_no_token_gets_a_fresh_unresumable_id() {
        let first = workflow_identity(7, None, &envelope(serde_json::json!({"author": "ada"})));
        let second = workflow_identity(7, None, &envelope(serde_json::json!({"author": "ada"})));
        assert_ne!(first.id, second.id, "two clicks must not dedupe by accident");
        assert!(!first.resumable);
    }

    /// A blank or whitespace token is no token. Treating `""` as an identity
    /// would collapse every such request in the process onto one log.
    #[test]
    fn a_blank_token_is_treated_as_absent() {
        for blank in ["", "   "] {
            let identity =
                workflow_identity(7, None, &envelope(serde_json::json!({INTENT_FIELD: blank})));
            assert!(
                !identity.resumable,
                "a blank token must not name a shared workflow"
            );
        }
    }

    /// A non-JSON payload (a click carries none) must not panic or resume.
    #[test]
    fn a_payload_that_is_not_json_falls_back_cleanly() {
        let click = ActionEnvelope {
            action_id: 7,
            event_kind: 0,
            payload: Vec::new(),
        };
        assert!(!workflow_identity(7, None, &click).resumable);
    }
}
