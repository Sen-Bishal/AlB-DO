//! SANDGATE · the confinement substrate — TODO 10.0-A, shipped.
//!
//! # What this is
//!
//! [`QuickJsEngine::rebuild_realm`] discards a poisoned realm and builds a fresh
//! one. Gate 1 proved the primitive is sound and leak-free; it did **not** give
//! anyone a way to use it, because a rebuilt realm has no modules and the caller
//! is left to remember what to put back. This module is that memory: a
//! **ledger** of every registration the engine has performed, in order, so a
//! realm can be reconstituted without the caller knowing anything.
//!
//! ```text
//!   register  ──► ledger.record(specifier, script, hash)
//!   confine() ──► rebuild_realm(|e| ledger.replay(e))
//! ```
//!
//! # Why the ledger stores the *script*, not the source
//!
//! `load_module_with_spec` runs the TSX through swc (parse → strip types → JSX
//! → lower to a module record) before evaluating anything. That work is
//! deterministic in the source, so replaying it per request would be pure
//! waste. The ledger keeps the **exact string that was evaluated**, which makes
//! replay a pure engine operation and keeps the compiler off the request path.
//!
//! # Bytecode
//!
//! SANDGATE gate 3.1 measured re-registering 55 Radix chunks at **4.415 ms**
//! through `ctx.eval` and **0.650 ms** through `Module::write` / `Module::load`
//! — 6.8×, and 71% of a confined request's cost. [`BytecodeCache`] does that
//! compile **once per distinct script** and reuses the bytes for every
//! subsequent replay on that engine.
//!
//! Three properties make this affordable *and* safe here, and all three are
//! load-bearing:
//!
//! 1. **The cache never leaves the process.** Gate 3.1 flagged an on-disk cache
//!    as a new integrity boundary — `Module::load` is `unsafe` because
//!    malformed bytecode is arbitrary behaviour inside the very realm SANDGATE
//!    exists to confine, so a tampered file would be code execution. Keeping
//!    the bytes in memory, produced by `Module::write` microseconds earlier,
//!    removes that boundary rather than defending it.
//! 2. **It is keyed by the script's content hash**, so a dev reload that
//!    changes a module simply misses and recompiles.
//! 3. **Version lock is automatic.** Bytecode is tied to the exact QuickJS
//!    build; because nothing is persisted, a rebuilt binary cannot meet a stale
//!    artifact.
//!
//! # The strict-mode boundary — the reason first registration is never bytecode
//!
//! `Module::declare` forces `JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_STRICT`. A
//! script that runs today in sloppy mode may therefore *parse* as a module and
//! still behave differently: an assignment to an undeclared identifier throws,
//! top-level `this` is `undefined`, and a function declared in module source
//! stays strict when it is called later.
//!
//! So the split is deliberate:
//!
//! * **First registration always takes today's `ctx.eval` path.** Semantics for
//!   application authors are untouched, whatever they wrote.
//! * **Only replay uses bytecode**, and only for the module classes that have
//!   been measured through it.
//! * A script that refuses to compile is remembered in [`BytecodeCache::refused`]
//!   and replayed as source forever after — a slower engine, never a broken one.
//!
//! `ALBEDO_SANDGATE_BYTECODE=0` disables the fast path wholesale, which is the
//! switch to reach for if a package ever turns out to depend on sloppy-mode
//! semantics inside a factory body.

// AUDITED EXCEPTION — `rquickjs::Module::load` is `unsafe` because QuickJS will
// execute whatever byte string it is handed, and malformed bytecode is
// arbitrary behaviour. Every byte this module loads was produced by
// `Module::write` on this same process's `Runtime`, is held only in this
// process's heap, and is keyed by the content hash of the script it came from.
// Nothing is read from disk, from the network, or from any input a request can
// influence. See the module docs for why an on-disk cache is deliberately not
// built.
#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use super::engine::stable_source_hash;

/// Above this, the shared store is emptied wholesale and refills lazily.
///
/// A cap rather than an eviction policy on purpose: entries are keyed by
/// content hash, so the only way the store grows without bound is a long dev
/// session editing modules, where nearly everything in it is stale anyway. An
/// LRU would be machinery for a case a `clear()` handles.
const BYTECODE_STORE_CAP_BYTES: usize = 64 * 1024 * 1024;

/// 📏 **One store for the whole process, not one per engine.**
///
/// Measured: 517 KB of bytecode for the Radix corpus plus the prelude. A pool
/// sizes itself to the machine's parallelism, so a per-engine cache multiplies
/// that by 8-16 — 4 to 8 MB of identical bytes. Sharing is sound because
/// QuickJS bytecode carries its own atom table: the bytes have no affinity to
/// the realm or the runtime that produced them, only to the engine *build*, and
/// nothing here is persisted across builds.
static BYTECODE_STORE: LazyLock<Mutex<BytecodeStore>> =
    LazyLock::new(|| Mutex::new(BytecodeStore::default()));

#[derive(Debug, Default)]
struct BytecodeStore {
    /// 🔑 **`Arc`, so a lookup can hand the bytes out and release the lock.**
    ///
    /// The first version returned a borrow, which meant the `MutexGuard` stayed
    /// alive across `Module::load` *and* `Module::eval` — i.e. across running
    /// JS. Every engine in the pool shares this store, so one engine replaying
    /// 55 artifacts would have held the lock through 55 module evaluations and
    /// serialised the entire pool behind itself. Cloning an `Arc` is a refcount
    /// bump; cloning the 413 KB of bytes per request would have been worse than
    /// the problem.
    compiled: HashMap<u64, Arc<Vec<u8>>>,
    refused: HashSet<u64>,
    resident: usize,
}

impl BytecodeStore {
    fn insert(&mut self, key: u64, bytes: Vec<u8>) -> Arc<Vec<u8>> {
        if self.resident + bytes.len() > BYTECODE_STORE_CAP_BYTES {
            self.compiled.clear();
            self.resident = 0;
        }
        self.resident += bytes.len();
        let bytes = Arc::new(bytes);
        self.compiled.insert(key, Arc::clone(&bytes));
        bytes
    }

    /// The bytes for `key`, if compiled. Returns an owned handle so the caller
    /// can drop the lock before touching QuickJS.
    fn get(&self, key: u64) -> Option<Arc<Vec<u8>>> {
        self.compiled.get(&key).map(Arc::clone)
    }
}

fn store() -> MutexGuard<'static, BytecodeStore> {
    // A poisoned store is recoverable: the bytes are a cache, and `insert` is
    // the only writer and is infallible, so a panic cannot have left a
    // half-written entry.
    BYTECODE_STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether a registration came from the application or from `node_modules`.
///
/// This is the granularity SANDGATE-A confines at. It is tracked in **Rust**,
/// never in the realm: a flag a package can reach is a flag a package can clear,
/// and the whole point of the dirty bit is that the package does not get a vote
/// on whether its realm is discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Compiled from the project's own source.
    Project,
    /// An npm artifact — third-party code the compiler did not write.
    ThirdParty,
}

impl Origin {
    /// True for code the project did not author.
    #[must_use]
    pub fn is_third_party(self) -> bool {
        matches!(self, Origin::ThirdParty)
    }
}

/// One registration, recorded exactly as it was evaluated.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    /// The module specifier the engine keys this registration under.
    pub specifier: String,
    /// The literal string that was evaluated. Replaying this reproduces the
    /// registration without re-entering the compiler.
    pub script: String,
    /// The engine's idempotency key for `(specifier, source)`, restored into
    /// `loaded_module_hashes` after a replay so memoisation still holds.
    pub hash: u64,
    /// Where the code came from.
    pub origin: Origin,
    /// Content hash of `script`, and the [`BytecodeCache`] key.
    pub script_hash: u64,
}

/// Counters describing what confinement has actually done on this engine.
///
/// Exposed because the alternative is believing it works. `confinements` moving
/// while `replayed_entries` stays at zero means the ledger is empty and the
/// rebuild is confining nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfinementStats {
    /// Realm rebuilds driven by [`ModuleLedger`].
    pub confinements: u64,
    /// Total registrations replayed across all confinements.
    pub replayed_entries: u64,
    /// Replays served from compiled bytecode.
    pub bytecode_hits: u64,
    /// Replays that fell back to evaluating the script as source.
    pub source_replays: u64,
    /// Distinct scripts that refused to compile to bytecode.
    pub bytecode_refusals: u64,
}

/// An engine's handle on the process-wide bytecode store.
///
/// Carries only the per-engine kill switch; the bytes live in
/// [`BYTECODE_STORE`]. See its docs for why sharing is both sound and
/// necessary.
#[derive(Debug, Clone)]
pub struct BytecodeCache {
    enabled: bool,
}

impl Default for BytecodeCache {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl BytecodeCache {
    /// A handle honouring `ALBEDO_SANDGATE_BYTECODE` (any value other than `0`
    /// leaves the fast path on).
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var("ALBEDO_SANDGATE_BYTECODE")
                .map(|value| value != "0")
                .unwrap_or(true),
        }
    }

    /// Number of scripts held as bytecode, **process-wide**.
    #[must_use]
    pub fn len(&self) -> usize {
        store().compiled.len()
    }

    /// True when no script has been compiled yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        store().compiled.is_empty()
    }

    /// Total bytes of bytecode resident, **process-wide**. Gate 3.1 measured
    /// 2.88× the source size for the npm artifacts alone; the prelude carries
    /// the ratio to ~3.6×.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        store().resident
    }

    /// Number of scripts that refused to compile and will always run as source.
    #[must_use]
    pub fn refusals(&self) -> usize {
        store().refused.len()
    }

    /// Whether the bytecode fast path is armed for this engine.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// The ordered record of every registration on one engine's realm.
///
/// Order is **first-registration order**, which is the order that worked: a
/// project module links its imports eagerly at load, so a dependency has to be
/// registered before its dependent, and the sequence the engine was actually
/// driven through is by construction such a sequence. Re-registering an
/// existing specifier updates the entry **in place** rather than moving it to
/// the back, because a dev reload changes a module's *body*, not its position
/// in the dependency graph.
#[derive(Debug, Default)]
pub struct ModuleLedger {
    entries: Vec<LedgerEntry>,
    index: HashMap<String, usize>,
    third_party_registered: bool,
    stats: ConfinementStats,
}

impl ModuleLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a registration, or update the one already held for `specifier`.
    pub fn record(&mut self, specifier: &str, script: &str, hash: u64, origin: Origin) {
        if origin.is_third_party() {
            self.third_party_registered = true;
        }
        let script_hash = stable_source_hash(script);
        if let Some(&position) = self.index.get(specifier) {
            let entry = &mut self.entries[position];
            entry.script.clear();
            entry.script.push_str(script);
            entry.hash = hash;
            entry.origin = origin;
            entry.script_hash = script_hash;
            return;
        }
        self.index.insert(specifier.to_string(), self.entries.len());
        self.entries.push(LedgerEntry {
            specifier: specifier.to_string(),
            script: script.to_string(),
            hash,
            origin,
            script_hash,
        });
    }

    /// Registrations in replay order.
    #[must_use]
    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    /// How many registrations the ledger holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing has been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True once any npm artifact has been registered on this realm.
    ///
    /// 🔑 This is the **dirty bit**, and it is deliberately coarse: an engine
    /// that has third-party code registered is treated as poisonable for every
    /// request, because "did that package's factory actually run this time" is
    /// a question only the realm can answer, and the realm is the thing under
    /// suspicion. Confining an engine that did not need it costs ~2.7 ms
    /// (gate 3.1); trusting a package's own account of whether it ran costs
    /// the guarantee.
    #[must_use]
    pub fn holds_third_party(&self) -> bool {
        self.third_party_registered
    }

    /// Bytes of registration script retained for replay.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.script.len()).sum()
    }

    /// Counters for what confinement has done.
    #[must_use]
    pub fn stats(&self) -> ConfinementStats {
        self.stats
    }

    /// Rebuild `loaded_module_hashes` from the ledger after a replay, so the
    /// engine's memoisation survives the realm it described.
    #[must_use]
    pub fn hash_table(&self) -> HashMap<String, u64> {
        self.entries
            .iter()
            .map(|entry| (entry.specifier.clone(), entry.hash))
            .collect()
    }

    /// Forget everything. Used when an engine's realm is rebuilt from a
    /// different project (a dev reload that swapped the world), where replaying
    /// the old world would resurrect modules that no longer exist.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
        self.third_party_registered = false;
        // The bytecode store is deliberately NOT dropped. It is shared by every
        // engine in the process and keyed by content hash, so an entry this
        // engine no longer needs is either still in use elsewhere or inert —
        // and clearing it here would evict another engine's warm cache to
        // reclaim bytes a dev reload will mostly re-add.
        // `BYTECODE_STORE_CAP_BYTES` is what bounds it.
    }

    pub(crate) fn note_confinement(&mut self, outcome: ReplayOutcome) {
        self.stats.confinements += 1;
        self.stats.replayed_entries += outcome.replayed;
        self.stats.bytecode_hits += outcome.bytecode_hits;
        self.stats.source_replays += outcome.source_replays;
        self.stats.bytecode_refusals = outcome.total_refusals;
    }
}

/// What one replay did, before it is folded into [`ConfinementStats`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ReplayOutcome {
    pub replayed: u64,
    pub bytecode_hits: u64,
    pub source_replays: u64,
    pub total_refusals: u64,
}

/// One named piece of the realm prelude.
///
/// Named because the error path is the point: *"failed to install the React
/// host records"* is actionable and *"failed to install prelude fragment 7"* is
/// not. The name is the same string the ten hand-written `map_err` closures
/// used to carry.
#[derive(Debug, Clone)]
pub struct PreludeFragment {
    /// Human-readable name, used verbatim in the initialisation error.
    pub name: &'static str,
    /// The script installed into a fresh realm.
    pub source: String,
    /// Content hash, and the [`BytecodeCache`] key.
    pub source_hash: u64,
}

impl PreludeFragment {
    /// Build a fragment, hashing its source for the bytecode cache.
    #[must_use]
    pub fn new(name: &'static str, source: String) -> Self {
        let source_hash = stable_source_hash(&source);
        Self {
            name,
            source,
            source_hash,
        }
    }
}

/// Install one prelude fragment, preferring cached bytecode.
///
/// # Why the prelude is eligible when a project module is not
///
/// The strict-mode hazard that keeps application code off this path does not
/// apply here: **every fragment is compiler-authored**, so "does this depend on
/// sloppy-mode semantics" is a question with an owner and an answer rather than
/// a bet on what a user wrote. The answer is checked rather than asserted —
/// `tests/sandgate_prelude_equivalence.rs` evaluates every fragment both ways
/// and compares the global surface each produces, which is the check that
/// catches the one real hazard: a top-level `const` in a fragment is a global
/// lexical binding as a script and a module-local one as a module, so a later
/// fragment that reads it would break silently.
///
/// This is the dominant term in a confined request — 72% of it
/// (`tests/sandgate_confine_cost.rs`) — which is what makes it worth the check.
///
/// # Errors
/// Propagates the underlying QuickJS error. A *compile* failure is not an
/// error: the fragment falls back to source and stays there.
pub(crate) fn eval_prelude_fragment(
    ctx: &rquickjs::Ctx<'_>,
    fragment: &PreludeFragment,
    cache: &mut BytecodeCache,
) -> Result<(), rquickjs::Error> {
    if !cache.enabled || store().refused.contains(&fragment.source_hash) {
        ctx.eval::<(), _>(fragment.source.as_str())?;
        return Ok(());
    }

    let mut cached = store().get(fragment.source_hash);
    if cached.is_none() {
        match rquickjs::Module::declare(ctx.clone(), fragment.name, fragment.source.as_str())
            .and_then(|module| module.write(false))
        {
            Ok(bytes) => {
                cached = Some(store().insert(fragment.source_hash, bytes));
            }
            Err(err) => {
                tracing::debug!(
                    target: "albedo.sandgate",
                    fragment = %fragment.name,
                    error = %err,
                    "prelude fragment does not compile to bytecode; installing as source"
                );
                store().refused.insert(fragment.source_hash);
                ctx.eval::<(), _>(fragment.source.as_str())?;
                return Ok(());
            }
        }
    }

    let bytes = cached.expect("bytecode compiled or found above");

    // SAFETY: as in `replay_entry` — produced by `Module::write` on this
    // process's own `Runtime`, keyed by the content hash of the source it came
    // from, never persisted and never reachable by a request. See the
    // module-level audit note.
    let module = unsafe { rquickjs::Module::load(ctx.clone(), bytes.as_slice()) }?;
    // A throwing body is a REJECTED PROMISE, not an `Err` — see `replay_entry`.
    let (_evaluated, promise) = module.eval()?;
    promise.finish::<()>()?;
    Ok(())
}

/// Which registrations are allowed to replay through bytecode.
///
/// npm artifacts are the measured case (gate 3.1: 55/55 Radix chunks, identical
/// factory + alias table) **and** the case that carries 71% of the cost. A
/// project module's script splices the author's own statements into an IIFE, so
/// promoting it to strict module source could change the meaning of code
/// somebody wrote — for a share of a cost that was never the problem.
fn eligible_for_bytecode(entry: &LedgerEntry) -> bool {
    entry.origin.is_third_party()
}

/// Evaluate one registration into `ctx`, preferring cached bytecode.
///
/// Returns `true` when the bytecode path served it.
///
/// # Errors
/// Propagates the underlying QuickJS error. A *compile* failure is not an
/// error — it demotes the script to source replay permanently.
pub(crate) fn replay_entry(
    ctx: &rquickjs::Ctx<'_>,
    entry: &LedgerEntry,
    cache: &mut BytecodeCache,
) -> Result<bool, rquickjs::Error> {
    if !cache.enabled
        || !eligible_for_bytecode(entry)
        || store().refused.contains(&entry.script_hash)
    {
        ctx.eval::<(), _>(entry.script.as_str())?;
        return Ok(false);
    }

    let mut cached = store().get(entry.script_hash);
    if cached.is_none() {
        match rquickjs::Module::declare(ctx.clone(), entry.specifier.as_str(), entry.script.as_str())
            .and_then(|module| module.write(false))
        {
            Ok(bytes) => {
                cached = Some(store().insert(entry.script_hash, bytes));
            }
            Err(err) => {
                // Not fatal, and not silent: the engine keeps working at the
                // slower speed and says which artifact opted out.
                tracing::debug!(
                    target: "albedo.sandgate",
                    specifier = %entry.specifier,
                    error = %err,
                    "registration script does not compile to bytecode; replaying as source"
                );
                store().refused.insert(entry.script_hash);
                ctx.eval::<(), _>(entry.script.as_str())?;
                return Ok(false);
            }
        }
    }

    // The lock is already released — `cached` owns its bytes. Nothing below
    // may hold it, because everything below runs JS.
    let bytes = cached.expect("bytecode compiled or found above");

    // SAFETY: `bytes` was produced by `Module::write` on this same process's
    // `Runtime` (immediately above, or on an earlier replay of this identical
    // script), has never left this process's heap, and is keyed by the content
    // hash of the script it was compiled from. There is no path by which a
    // request, a file, or a package can substitute these bytes. See the
    // module-level audit note.
    let module = unsafe { rquickjs::Module::load(ctx.clone(), bytes.as_slice()) }?;

    // 🔴 **`Module::eval` reports a throwing module body as a REJECTED PROMISE,
    // not as an `Err`.** `module.eval()?` alone therefore returns `Ok` for a
    // registration that did nothing, and the engine comes back missing that
    // module with nothing logged — the exact failure gate 3.1's harness hit and
    // spent two false measurements on. `finish` drives the pending job queue
    // and surfaces the rejection as the error it is.
    //
    // Costs nothing for the ordinary case: these bodies are synchronous, so the
    // promise is already resolved when it is handed back.
    let (_evaluated, promise) = module.eval()?;
    promise.finish::<()>()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_the_same_specifier_updates_in_place_and_keeps_its_position() {
        let mut ledger = ModuleLedger::new();
        ledger.record("a", "SCRIPT_A", 1, Origin::Project);
        ledger.record("b", "SCRIPT_B", 2, Origin::Project);
        ledger.record("a", "SCRIPT_A2", 3, Origin::Project);

        let specs: Vec<&str> = ledger
            .entries()
            .iter()
            .map(|entry| entry.specifier.as_str())
            .collect();
        assert_eq!(
            specs,
            vec!["a", "b"],
            "a re-registration must not move a module behind something that imports it"
        );
        assert_eq!(ledger.entries()[0].script, "SCRIPT_A2");
        assert_eq!(ledger.entries()[0].hash, 3);
    }

    #[test]
    fn the_dirty_bit_latches_on_the_first_third_party_registration() {
        let mut ledger = ModuleLedger::new();
        ledger.record("app", "S", 1, Origin::Project);
        assert!(!ledger.holds_third_party());
        ledger.record("node_modules/x", "S", 2, Origin::ThirdParty);
        assert!(ledger.holds_third_party());
        // and it does not un-latch when more project code is registered
        ledger.record("app2", "S", 3, Origin::Project);
        assert!(ledger.holds_third_party());
    }

    #[test]
    fn only_third_party_registrations_take_the_bytecode_path() {
        let project = LedgerEntry {
            specifier: "routes/index.tsx".into(),
            script: "1".into(),
            hash: 0,
            origin: Origin::Project,
            script_hash: 0,
        };
        let npm = LedgerEntry {
            origin: Origin::ThirdParty,
            ..project.clone()
        };
        assert!(!eligible_for_bytecode(&project));
        assert!(eligible_for_bytecode(&npm));
    }

    /// 🔴 Regression test for a bug this module shipped and then found in
    /// review: `Module::eval()?` returns `Ok` for a module whose body throws,
    /// because the throw lands in the returned promise. A replay that silently
    /// registered nothing would leave the realm short a module and say nothing
    /// — `project_silent_island_death`'s shape, in a new place.
    #[test]
    fn a_replayed_module_that_throws_is_an_error_and_not_a_silent_no_op() {
        let runtime = rquickjs::Runtime::new().expect("runtime");
        let context = rquickjs::Context::full(&runtime).expect("context");
        let mut cache = BytecodeCache::from_env();
        let entry = LedgerEntry {
            specifier: "npm:boom@1.0.0/index.js".into(),
            script: "throw new Error('replay boom');".into(),
            hash: 1,
            origin: Origin::ThirdParty,
            script_hash: stable_source_hash("throw new Error('replay boom');"),
        };
        context.with(|ctx| {
            let result = replay_entry(&ctx, &entry, &mut cache);
            assert!(
                result.is_err(),
                "a module body that throws during replay reported success"
            );
        });
    }

    #[test]
    fn the_hash_table_round_trips_the_engines_memoisation_keys() {
        let mut ledger = ModuleLedger::new();
        ledger.record("a", "SA", 11, Origin::Project);
        ledger.record("b", "SB", 22, Origin::ThirdParty);
        let table = ledger.hash_table();
        assert_eq!(table.get("a"), Some(&11));
        assert_eq!(table.get("b"), Some(&22));
        assert_eq!(table.len(), 2);
    }
}

/// **SANDGATE-B · the sealed intrinsics.** Installed first, before anything
/// else touches the realm.
///
/// # The attack this exists to close
///
/// Gate 2 left one row open, and it is the one that matters: a package patches
/// `JSON.stringify`, and the handler epilogue — which serialises the effect
/// list through exactly that function — hands the patched version an array it
/// is free to rewrite. The package never needs to reach `append`; it only needs
/// to be on the call path when the effects are encoded. Confinement does not
/// touch this, because the package is re-imported by the very route it attacks
/// and re-applies the patch *before* the handler runs.
///
/// ```js
/// var real = JSON.stringify;
/// JSON.stringify = function (v) {
///   if (Array.isArray(v)) { v = v.concat([{ kind: 'forge_append', … }]); }
///   return real.apply(JSON, arguments);
/// };
/// ```
///
/// # The shape of the fix
///
/// Capture the intrinsics the trust boundary depends on **before any
/// third-party code can run**, and put them somewhere a package cannot reach:
///
/// * the property is installed with `writable: false, configurable: false`, so
///   assignment fails and `defineProperty` throws;
/// * the object is `Object.freeze`d and has a **null prototype**, so neither
///   its entries nor `Object.prototype` can be used to shadow them;
/// * the provenance stack it carries is a closure variable with a
///   null-prototype backing object, so no `Array.prototype` index setter or
///   inherited accessor can observe or rewrite it.
///
/// # What is still reachable, stated precisely
///
/// `JSON.stringify` consults `toJSON` on any **object** it serialises. A
/// package that plants `Object.prototype.toJSON` can therefore still corrupt an
/// effect's *payload* — a value the application authored and the attacker's own
/// request is entitled to influence anyway. It cannot manufacture, delete, or
/// retarget an effect, because the effect list is assembled by string
/// concatenation over pre-encoded entries and never passes through `stringify`
/// as an object.
///
/// [`integrity_probe_expression`] closes even that, by refusing any handler run
/// in a realm where such a hook is present — no application plants `toJSON` on
/// `Object.prototype`, so a false positive is an attack by another name.
#[must_use]
pub fn build_sealed_intrinsics_script() -> String {
    // Written as a single sloppy-mode IIFE so it can be `ctx.eval`'d like every
    // other prelude fragment. Nothing here depends on the realm having anything
    // installed yet — that is the point.
    r#"
(function () {
  if (typeof globalThis.__albedo_sealed !== 'undefined') { return; }

  var S = Object.create(null);

  // ── pristine intrinsics ────────────────────────────────────────────────
  S.stringify   = JSON.stringify;
  S.parse       = JSON.parse;
  S.isArray     = Array.isArray;
  S.keys        = Object.keys;
  S.create      = Object.create;
  S.getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
  var _hasOwn   = Object.prototype.hasOwnProperty;
  var _call     = Function.prototype.call;
  S.hasOwn      = function (o, k) { return _call.call(_hasOwn, o, k); };

  // A null-prototype record. Effects are built as these so that serialising one
  // cannot pick up an inherited `toJSON`.
  S.record = function () { return Object.create(null); };

  // ── provenance (SANDGATE-B) ────────────────────────────────────────────
  //
  // The stack is a null-prototype object plus a closure counter rather than an
  // array: an array index assignment walks `Array.prototype` for a setter, and
  // a package can install one.
  var _frames = Object.create(null);
  var _depth = 0;
  S.enterModule = function (key) { _frames[_depth] = String(key); _depth = _depth + 1; };
  S.exitModule  = function () { if (_depth > 0) { _depth = _depth - 1; _frames[_depth] = null; } };
  S.currentOrigin = function () { return _depth === 0 ? null : _frames[_depth - 1]; };
  S.originDepth = function () { return _depth; };

  // A one-way latch: third-party code executed in this realm.
  //
  // The only operation offered is "set it". There is no clear, and `_ran` is a
  // closure variable on a frozen, non-configurable holder, so a package can
  // make the server confine MORE often (harmless) and can never make it confine
  // less. A flag a package could clear would be worse than no flag.
  var _ran = false;
  S.markThirdPartyRan = function () { _ran = true; };
  S.thirdPartyRan = function () { return _ran; };

  // ⚠️ `enterModule` is callable by anything in the realm, including a package
  // — so a hostile package can push a frame, never pop it, and make every
  // effect in that realm carry an origin and be refused.
  //
  // That is accepted, not overlooked. A package that wants to deny service can
  // simply `throw` at module top level and take every render with it; this adds
  // no capability it did not already have. What matters is that the stack is
  // NOT load-bearing for integrity — that is carried by the pristine encoder
  // above — so abusing it can only cause refusals, never forgeries.

  // ── integrity probe ────────────────────────────────────────────────────
  //
  // Returns null when the realm is clean, or a short reason when a hook that
  // could rewrite a serialised payload is present. Read on every handler run.
  //
  // 🔑 Probed with `getOwnPropertyDescriptor`, not by reading the property. A
  // plain read invokes an accessor with `Object.prototype` as the receiver, so
  // `Object.defineProperty(Object.prototype, 'toJSON', { get() { return this ===
  // Object.prototype ? undefined : hijack; } })` would answer "clean" here and
  // "hijack" to `JSON.stringify`, whose read uses the *value* as receiver.
  // A descriptor is present either way.
  var _protos = [
    ['Object.prototype.toJSON', Object.prototype],
    ['Array.prototype.toJSON', Array.prototype],
    ['String.prototype.toJSON', String.prototype],
    ['Number.prototype.toJSON', Number.prototype],
    ['Boolean.prototype.toJSON', Boolean.prototype]
  ];
  S.integrity = function () {
    for (var i = 0; i < _protos.length; i++) {
      if (Object.getOwnPropertyDescriptor(_protos[i][1], 'toJSON') !== undefined) {
        return _protos[i][0];
      }
    }
    return null;
  };

  Object.freeze(S);
  Object.defineProperty(globalThis, '__albedo_sealed', {
    value: S,
    writable: false,
    enumerable: false,
    configurable: false
  });
})();
"#
    .to_string()
}

/// The JS expression a trust-boundary envelope embeds to report realm
/// integrity. Split out so the handler path and any future boundary use the
/// same probe rather than two spellings of it.
#[must_use]
pub fn integrity_probe_expression() -> &'static str {
    "globalThis.__albedo_sealed.integrity()"
}

#[cfg(test)]
mod sealed_tests {
    use super::*;

    #[test]
    fn the_sealed_script_installs_a_non_configurable_global() {
        let script = build_sealed_intrinsics_script();
        assert!(script.contains("configurable: false"));
        assert!(script.contains("writable: false"));
        assert!(
            script.contains("Object.freeze(S)"),
            "an unfrozen holder lets a package swap `stringify` inside it"
        );
        assert!(
            script.contains("Object.create(null)"),
            "the holder must have a null prototype or Object.prototype shadows it"
        );
    }

    #[test]
    fn the_integrity_probe_names_every_prototype_that_can_hook_serialisation() {
        let script = build_sealed_intrinsics_script();
        for hook in [
            "Object.prototype.toJSON",
            "Array.prototype.toJSON",
            "String.prototype.toJSON",
            "Number.prototype.toJSON",
            "Boolean.prototype.toJSON",
        ] {
            assert!(script.contains(hook), "integrity probe misses {hook}");
        }
        assert!(
            script.contains("Object.getOwnPropertyDescriptor(_protos[i][1], 'toJSON')"),
            "the probe must read a DESCRIPTOR: a plain property read is answerable by a \
             receiver-dependent getter that lies to the probe and hijacks `stringify`"
        );
    }
}
