//! R1b · the live-connection registry.
//!
//! Every live connection — a PHOSPHOR lane, an SSE patch stream, a WebTransport
//! session — registers here while it is open, so whatever ends a session or
//! changes a principal's roles can close the connections that were admitted
//! under the old authority.
//!
//! ## The two rules that leave no gap
//!
//! - Every invalidator **commits, then scans** this registry.
//! - Every open **registers, then reads**.
//!
//! So for any invalidation racing any open, either the open's read sees the
//! commit, or the invalidator's scan sees the entry.
//!
//! ## The read is the request's own session lookup
//!
//! SSE and PHOSPHOR register **before** identity resolution
//! ([`LiveRegistry::register_presented`], keyed by the presented cookie's token
//! hash), so the session lookup the request makes anyway *is* the read — no
//! second query per open ([`LiveRegistry::admit`]).
//!
//! The one thing that lookup cannot fence on its own is a role change that
//! commits after the read but scans before the entry knows its principal. A
//! counter closes that: every role-change scan bumps [`LiveRegistry`]'s
//! authority generation under a mutex before scanning, and an open reads it
//! under the same mutex before its read and again after publishing its
//! principal. Unchanged → any later scan sees the published principal. Changed
//! → the open falls back to the explicit re-validation query. Session ends need
//! no counter: they close by token hash, which the entry carries from the start.
//!
//! WebTransport binds long after its identity was resolved, so it registers
//! resolved and re-validates with the query ([`LiveRegistry::revalidate`]) —
//! once per QUIC session.
//!
//! ## What is closed, exactly
//!
//! - Session ends close by **token hash, from the set the store deleted in that transaction** —
//!   never a broad "this principal's connections" predicate, which would also close a session
//!   minted after the commit. Looked up through a token index: O(matches), not O(connections).
//! - Role changes close entries of that principal **admitted below** the new epoch, through a
//!   principal index.
//! - Expiry closes entries whose admitted session expiry has passed (the sweep tick).
//!
//! Anonymous entries are never re-validated and never matched by any of these:
//! public live data keeps flowing as it did before.
//!
//! ## The cutoff
//!
//! Invalidation linearises at the atomic store of [`LiveState::Closed`]. An
//! emitter loads the state immediately before handing a frame to the response
//! body or the QUIC stream ([`LiveEntry::admits`]); a frame handed off after a
//! load that saw `Live` may still reach the client, nothing after the close
//! does. `Closed` is terminal: [`LiveEntry::promote`] is a compare-and-swap that
//! fails once the entry is closed.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use dashmap::DashMap;
use dom_render_compiler::auth::principal::PrincipalId;
use dom_render_compiler::auth::session::TokenHash;
use dom_render_compiler::auth::store;
use dom_render_compiler::forge::DataSubstrate;
use tokio::sync::Notify;

use crate::auth::Identity;

/// Which transport an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveKind {
    /// A PHOSPHOR trunk.
    Phosphor,
    /// A standalone SSE patch stream.
    Sse,
    /// A WebTransport session.
    WebTransport,
}

/// An entry's lifecycle. Stored in an `AtomicU8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LiveState {
    /// Registered; not admitted yet. Emits nothing protected.
    Opening = 0,
    /// Admitted under current authority.
    Live = 1,
    /// Closed. Terminal.
    Closed = 2,
}

impl LiveState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Opening,
            1 => Self::Live,
            _ => Self::Closed,
        }
    }
}

/// What an authenticated entry was admitted with.
#[derive(Debug, Clone)]
pub struct Admitted {
    /// Whose connection this is.
    pub principal: PrincipalId,
    /// The session token's hash.
    pub token: TokenHash,
    /// The authority epoch the connection's roles were read at.
    pub epoch: i64,
    /// When the admitting session expires, epoch ms.
    pub expires_at: i64,
}

type CloseHook = Box<dyn FnOnce() + Send>;

/// One registered live connection.
pub struct LiveEntry {
    id: u64,
    kind: LiveKind,
    state: AtomicU8,
    /// The presented session token's hash, known from registration. `None`
    /// when no cookie was presented.
    token: Option<TokenHash>,
    /// Filled once, on admission, when the identity is authenticated.
    admitted: OnceLock<Admitted>,
    /// Fires once when the entry closes. `notify_one` leaves a permit, so an
    /// emitter between polls still sees it.
    closed: Notify,
    /// Teardown to run on close (removing subscriptions installed earlier).
    hooks: Mutex<Vec<CloseHook>>,
}

impl std::fmt::Debug for LiveEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveEntry")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("state", &self.state())
            .field("authenticated", &self.admitted.get().is_some())
            .finish()
    }
}

impl LiveEntry {
    /// The entry's current state.
    #[must_use]
    pub fn state(&self) -> LiveState {
        LiveState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// **Handoff admission.** Load immediately before handing a protected frame
    /// to the response body or the QUIC send stream; hand it off only on `true`.
    #[inline]
    #[must_use]
    pub fn admits(&self) -> bool {
        self.state.load(Ordering::Acquire) == LiveState::Live as u8
    }

    /// Whether the entry has closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state() == LiveState::Closed
    }

    /// The transport.
    #[must_use]
    pub fn kind(&self) -> LiveKind {
        self.kind
    }

    /// What it was admitted with, when authenticated.
    #[must_use]
    pub fn admitted(&self) -> Option<&Admitted> {
        self.admitted.get()
    }

    /// `Opening → Live`. Fails, and leaves the entry closed, once it is closed.
    pub fn promote(&self) -> bool {
        self.state
            .compare_exchange(
                LiveState::Opening as u8,
                LiveState::Live as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Close. Idempotent; returns `true` only for the call that closed it.
    pub fn close(&self) -> bool {
        let previous = self.state.swap(LiveState::Closed as u8, Ordering::AcqRel);
        if previous == LiveState::Closed as u8 {
            return false;
        }
        self.closed.notify_one();
        let hooks = std::mem::take(&mut *self.hooks.lock().expect("live hooks poisoned"));
        for hook in hooks {
            hook();
        }
        true
    }

    /// Resolves once the entry is closed.
    pub async fn wait_closed(&self) {
        if self.is_closed() {
            return;
        }
        self.closed.notified().await;
    }

    /// Run `hook` when the entry closes — or now, if it already has.
    ///
    /// Installation and close coordinate under the hooks lock: a hook added
    /// after `close` took the list runs here instead, so a subscription
    /// installed across an await can never outlive a close it raced.
    pub fn on_close(&self, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = self.hooks.lock().expect("live hooks poisoned");
        if self.is_closed() {
            drop(hooks);
            hook();
            return;
        }
        hooks.push(Box::new(hook));
    }
}

/// Removes the entry from the registry when dropped — retention is exactly the
/// connection's lifetime. Dropping also closes it, so a cancelled open leaves
/// nothing behind.
pub struct LiveHandle {
    registry: Arc<LiveRegistry>,
    entry: Arc<LiveEntry>,
    /// The authority generation read at registration, before the request's
    /// session lookup (see the module docs).
    generation: u64,
}

impl LiveHandle {
    /// The entry.
    #[must_use]
    pub fn entry(&self) -> &Arc<LiveEntry> {
        &self.entry
    }
}

impl Drop for LiveHandle {
    fn drop(&mut self) {
        self.entry.close();
        self.registry.remove(&self.entry);
    }
}

/// A point an open can be paused at, for the race tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Checkpoint {
    /// Registered, before the re-validation query.
    BeforeRevalidate,
    /// The re-validation query returned valid, before the promote.
    BeforePromote,
    /// [`LiveRegistry::admit`]: the principal is published, before the
    /// generation re-check.
    BeforeGenerationCheck,
}

/// Test seam: a pause installed at a checkpoint. The open signals `reached`,
/// then waits for `release`.
#[derive(Default)]
pub struct Pause {
    /// Notified when an open reaches the checkpoint.
    pub reached: Notify,
    /// Notify to let it continue.
    pub release: Notify,
}

/// How an admission went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Live, on the request's own read.
    Fast,
    /// Live, after the generation moved and the explicit query confirmed it.
    Rechecked,
    /// Closed.
    Refused,
}

impl Admission {
    /// Whether the entry is live.
    #[must_use]
    pub fn admitted(self) -> bool {
        !matches!(self, Self::Refused)
    }
}

/// Indexes are small vectors of entry ids: one token or principal almost never
/// has more than a handful of live connections.
type Ids = smallvec_lite::Ids;

mod smallvec_lite {
    /// A tiny id list. One token or principal rarely has more than a few live
    /// connections, so a linear `Vec` beats a set here.
    #[derive(Default)]
    pub struct Ids(Vec<u64>);

    impl Ids {
        pub fn push(&mut self, id: u64) {
            self.0.push(id);
        }
        pub fn remove(&mut self, id: u64) {
            if let Some(at) = self.0.iter().position(|x| *x == id) {
                self.0.swap_remove(at);
            }
        }
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
        pub fn snapshot(&self) -> Vec<u64> {
            self.0.clone()
        }
    }
}

/// The registry.
#[derive(Default)]
pub struct LiveRegistry {
    next_id: AtomicU64,
    entries: DashMap<u64, Arc<LiveEntry>>,
    /// Token hash → entries presenting it. Session ends look up here.
    by_token: DashMap<TokenHash, Ids>,
    /// Principal → admitted entries. Role changes look up here.
    by_principal: DashMap<String, Ids>,
    /// Entries that presented a session cookie — what the sweep checks before
    /// spending a query.
    with_token: AtomicUsize,
    /// Bumped (under this mutex) before every role-change scan; read (under it)
    /// by every fast admission before its read and after publishing.
    generation: Mutex<u64>,
    /// Authenticated entries ordered by admitted expiry, so a tick costs
    /// O(expired), not O(connections).
    expiries: Mutex<BTreeSet<(i64, u64)>>,
    pauses: DashMap<Checkpoint, Arc<Pause>>,
}

impl LiveRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether none are.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn generation(&self) -> u64 {
        *self.generation.lock().expect("live generation poisoned")
    }

    fn insert(self: &Arc<Self>, kind: LiveKind, token: Option<TokenHash>) -> LiveHandle {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(token) = &token {
            self.by_token.entry(token.clone()).or_default().push(id);
            self.with_token.fetch_add(1, Ordering::Relaxed);
        }
        let entry = Arc::new(LiveEntry {
            id,
            kind,
            state: AtomicU8::new(LiveState::Opening as u8),
            token,
            admitted: OnceLock::new(),
            closed: Notify::new(),
            hooks: Mutex::new(Vec::new()),
        });
        self.entries.insert(id, entry.clone());
        // Read after the entry is visible: an invalidation whose bump this
        // misses will find the entry in its scan.
        let generation = self.generation();
        LiveHandle {
            registry: self.clone(),
            entry,
            generation,
        }
    }

    /// Register an SSE or PHOSPHOR open **before** its identity is resolved,
    /// keyed by the presented cookie's token hash (`None` without one). Then
    /// resolve and call [`Self::admit`].
    #[must_use]
    pub fn register_presented(
        self: &Arc<Self>,
        kind: LiveKind,
        token: Option<TokenHash>,
    ) -> LiveHandle {
        self.insert(kind, token)
    }

    /// Register a connection whose identity was resolved **before** this call
    /// (WebTransport binding, tests). Follow with [`Self::revalidate`].
    #[must_use]
    pub fn register(self: &Arc<Self>, kind: LiveKind, identity: &Identity) -> LiveHandle {
        let handle = self.insert(kind, identity.token().cloned());
        self.publish(&handle.entry, identity);
        handle
    }

    /// Publish an authenticated identity on the entry and index it.
    fn publish(&self, entry: &LiveEntry, identity: &Identity) {
        let Identity::Authenticated { principal, token } = identity else {
            return;
        };
        let admitted = Admitted {
            principal: principal.id.clone(),
            token: token.clone(),
            epoch: principal.authority_epoch,
            // A principal resolved from a session always carries its expiry.
            expires_at: principal.session_expires_at.unwrap_or(i64::MAX),
        };
        let expires_at = admitted.expires_at;
        let who = admitted.principal.as_str().to_string();
        if entry.admitted.set(admitted).is_err() {
            return;
        }
        self.by_principal.entry(who).or_default().push(entry.id);
        if expires_at != i64::MAX {
            self.expiries
                .lock()
                .expect("live expiries poisoned")
                .insert((expires_at, entry.id));
        }
    }

    fn remove(&self, entry: &LiveEntry) {
        if self.entries.remove(&entry.id).is_none() {
            return;
        }
        if let Some(token) = &entry.token {
            self.with_token.fetch_sub(1, Ordering::Relaxed);
            if let dashmap::mapref::entry::Entry::Occupied(mut slot) =
                self.by_token.entry(token.clone())
            {
                slot.get_mut().remove(entry.id);
                if slot.get().is_empty() {
                    slot.remove();
                }
            }
        }
        if let Some(admitted) = entry.admitted.get() {
            if let dashmap::mapref::entry::Entry::Occupied(mut slot) = self
                .by_principal
                .entry(admitted.principal.as_str().to_string())
            {
                slot.get_mut().remove(entry.id);
                if slot.get().is_empty() {
                    slot.remove();
                }
            }
            if admitted.expires_at != i64::MAX {
                self.expiries
                    .lock()
                    .expect("live expiries poisoned")
                    .remove(&(admitted.expires_at, entry.id));
            }
        }
    }

    /// Install a pause at `at` (tests). Returns it.
    pub fn pause_at(&self, at: Checkpoint) -> Arc<Pause> {
        let pause = Arc::new(Pause::default());
        self.pauses.insert(at, pause.clone());
        pause
    }

    /// Remove the pause at `at`.
    pub fn clear_pause(&self, at: Checkpoint) {
        self.pauses.remove(&at);
    }

    async fn checkpoint(&self, at: Checkpoint) {
        if self.pauses.is_empty() {
            return;
        }
        let pause = self.pauses.get(&at).map(|p| p.clone());
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.release.notified().await;
        }
    }

    /// Admit an entry registered with [`Self::register_presented`], given the
    /// identity the request resolved **after** registering.
    ///
    /// That resolution was the read. An anonymous identity goes live at once.
    /// An authenticated one is published and indexed; if no role-change scan
    /// ran since registration, it goes live with no further query, otherwise it
    /// falls back to [`Self::revalidate`].
    pub async fn admit(
        &self,
        handle: &LiveHandle,
        identity: &Identity,
        db: Option<&dyn DataSubstrate>,
    ) -> Admission {
        let entry = &handle.entry;
        match identity {
            Identity::Anonymous => {
                return if entry.promote() {
                    Admission::Fast
                } else {
                    Admission::Refused
                };
            }
            Identity::Authenticated { token, .. } => {
                // The entry is indexed under the presented token; an identity
                // resolved from anything else is not the one that registered.
                if entry.token.as_ref() != Some(token) {
                    entry.close();
                    return Admission::Refused;
                }
            }
        }
        self.publish(entry, identity);
        self.checkpoint(Checkpoint::BeforeGenerationCheck).await;
        if self.generation() == handle.generation {
            return if entry.promote() {
                Admission::Fast
            } else {
                Admission::Refused
            };
        }
        if self.revalidate(entry, db).await {
            Admission::Rechecked
        } else {
            Admission::Refused
        }
    }

    /// **Register, then read** with an explicit query: promoted only when the
    /// session row is still present and unexpired and the principal's epoch has
    /// not moved past the admitted one; otherwise — or when the query fails —
    /// closed (fail closed). Anonymous entries go straight to `Live`. Returns
    /// whether the entry is `Live`.
    pub async fn revalidate(&self, entry: &LiveEntry, db: Option<&dyn DataSubstrate>) -> bool {
        let Some(admitted) = entry.admitted.get() else {
            return entry.promote();
        };
        self.checkpoint(Checkpoint::BeforeRevalidate).await;
        let Some(db) = db else {
            entry.close();
            return false;
        };
        let now = crate::auth::now_ms();
        match store::live_check(db, &admitted.token, &admitted.principal, now).await {
            Ok(check)
                if check.session_live
                    && check.epoch <= admitted.epoch
                    && admitted.expires_at > now =>
            {
                self.checkpoint(Checkpoint::BeforePromote).await;
                entry.promote()
            }
            Ok(_) => {
                entry.close();
                false
            }
            Err(err) => {
                tracing::warn!(target: "albedo.auth", %err, "live re-validation failed; closing");
                entry.close();
                false
            }
        }
    }

    fn close_ids(
        &self,
        ids: impl IntoIterator<Item = u64>,
        matches: impl Fn(&LiveEntry) -> bool,
    ) -> usize {
        // Collect first: closing runs hooks, and hooks must not run under a
        // registry shard lock.
        let hits: Vec<Arc<LiveEntry>> = ids
            .into_iter()
            .filter_map(|id| self.entries.get(&id).map(|e| e.clone()))
            .filter(|entry| matches(entry))
            .collect();
        hits.into_iter().filter(|entry| entry.close()).count()
    }

    /// Close every entry presenting a token in `deleted` — the set a store call
    /// deleted in its transaction. Call after the commit, and only on success.
    /// Returns how many closed. O(matches).
    pub fn close_tokens(&self, deleted: &[TokenHash]) -> usize {
        let mut ids = Vec::new();
        for token in deleted {
            if let Some(found) = self.by_token.get(token) {
                ids.extend(found.snapshot());
            }
        }
        self.close_ids(ids, |_| true)
    }

    /// Close `principal`'s entries admitted below `epoch`. Returns how many.
    /// Bumps the authority generation first (see the module docs). O(matches).
    pub fn close_principal_below(&self, principal: &str, epoch: i64) -> usize {
        *self.generation.lock().expect("live generation poisoned") += 1;
        let ids = self
            .by_principal
            .get(principal)
            .map(|found| found.snapshot())
            .unwrap_or_default();
        self.close_ids(ids, |entry| {
            entry
                .admitted
                .get()
                .is_some_and(|admitted| admitted.epoch < epoch)
        })
    }

    /// Close entries whose admitted expiry is at or before `now`.
    pub fn close_expired(&self, now: i64) -> usize {
        let due: Vec<u64> = {
            let mut expiries = self.expiries.lock().expect("live expiries poisoned");
            let mut due = Vec::new();
            while let Some(&(at, id)) = expiries.first() {
                if at > now {
                    break;
                }
                expiries.pop_first();
                due.push(id);
            }
            due
        };
        self.close_ids(due, |_| true)
    }

    /// One sweep tick: expiry first (it needs no database), then every
    /// authority change after `watermark`. Returns the new watermark — the old
    /// one when the query failed, so nothing is skipped. With no
    /// cookie-bearing connection open there is nothing a change could close, so
    /// the query is skipped and the watermark kept; the next tick that has one
    /// catches up.
    pub async fn sweep_tick(
        &self,
        db: &dyn DataSubstrate,
        watermark: i64,
    ) -> (i64, Result<(), String>) {
        self.close_expired(crate::auth::now_ms());
        if self.with_token.load(Ordering::Relaxed) == 0 {
            return (watermark, Ok(()));
        }
        match store::authority_changes_after(db, watermark).await {
            Ok(changes) => {
                let mut next = watermark;
                for change in changes {
                    self.close_principal_below(&change.principal, change.epoch);
                    next = next.max(change.seq);
                }
                (next, Ok(()))
            }
            Err(err) => (watermark, Err(err.to_string())),
        }
    }
}

/// How often the sweep runs.
pub const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Read the starting watermark (before accepting traffic) and spawn the sweep.
///
/// # Errors
/// When the clock cannot be read — the server must not start without one, or a
/// restart would either close everything or miss changes.
pub async fn start_sweep(
    registry: Arc<LiveRegistry>,
    db: Arc<dyn DataSubstrate>,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let start = store::authority_clock(db.as_ref())
        .await
        .map_err(|err| format!("could not read the authority clock: {err}"))?;
    Ok(tokio::spawn(async move {
        let mut watermark = start;
        let mut failures: u32 = 0;
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let (next, outcome) = registry.sweep_tick(db.as_ref(), watermark).await;
            watermark = next;
            match outcome {
                Ok(()) => failures = 0,
                Err(err) => {
                    failures += 1;
                    if failures >= 3 {
                        tracing::error!(target: "albedo.auth", %err, failures, "authority sweep keeps failing");
                    } else {
                        tracing::warn!(target: "albedo.auth", %err, "authority sweep failed; will retry");
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::auth::principal::Principal;

    fn authed(principal: &str, token: &str, epoch: i64, expires_at: i64) -> Identity {
        let mut p = Principal::new(
            PrincipalId::parse(principal.to_string()).unwrap(),
            "password",
        );
        p.authority_epoch = epoch;
        p.session_expires_at = Some(expires_at);
        Identity::Authenticated {
            principal: Box::new(p),
            token: TokenHash::of(token),
        }
    }

    #[test]
    fn closed_is_terminal_and_promote_cannot_reopen() {
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register(LiveKind::Sse, &authed("u_a", "t", 0, i64::MAX - 1));
        assert!(handle.entry().close());
        assert!(
            !handle.entry().promote(),
            "Opening -> Live must fail after Closed"
        );
        assert_eq!(handle.entry().state(), LiveState::Closed);
        assert!(!handle.entry().admits());
    }

    #[test]
    fn close_tokens_closes_exactly_the_deleted_set() {
        let registry = Arc::new(LiveRegistry::new());
        let old = registry.register(LiveKind::Sse, &authed("u_a", "old", 0, i64::MAX - 1));
        let new = registry.register(LiveKind::Sse, &authed("u_a", "new", 0, i64::MAX - 1));
        let anon = registry.register(LiveKind::Sse, &Identity::Anonymous);
        for h in [&old, &new, &anon] {
            h.entry().promote();
        }
        assert_eq!(registry.close_tokens(&[TokenHash::of("old")]), 1);
        assert!(old.entry().is_closed());
        assert!(
            new.entry().admits(),
            "a session not in the deleted set survives"
        );
        assert!(anon.entry().admits(), "anonymous entries are never matched");
    }

    #[test]
    fn role_change_closes_only_entries_admitted_below_the_new_epoch() {
        let registry = Arc::new(LiveRegistry::new());
        let stale = registry.register(LiveKind::Phosphor, &authed("u_a", "t1", 1, i64::MAX - 1));
        let fresh = registry.register(LiveKind::Phosphor, &authed("u_a", "t2", 2, i64::MAX - 1));
        let other = registry.register(LiveKind::Phosphor, &authed("u_b", "t3", 0, i64::MAX - 1));
        for h in [&stale, &fresh, &other] {
            h.entry().promote();
        }
        assert_eq!(registry.close_principal_below("u_a", 2), 1);
        assert!(stale.entry().is_closed());
        assert!(fresh.entry().admits());
        assert!(other.entry().admits());
    }

    #[test]
    fn expiry_closes_due_entries_only() {
        let registry = Arc::new(LiveRegistry::new());
        let due = registry.register(LiveKind::Sse, &authed("u_a", "t1", 0, 1_000));
        let later = registry.register(LiveKind::Sse, &authed("u_a", "t2", 0, 5_000));
        due.entry().promote();
        later.entry().promote();
        assert_eq!(registry.close_expired(1_000), 1);
        assert!(due.entry().is_closed());
        assert!(later.entry().admits());
    }

    #[test]
    fn a_hook_installed_after_close_runs_immediately() {
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register(LiveKind::WebTransport, &Identity::Anonymous);
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle.entry().close();
        let flag = ran.clone();
        handle
            .entry()
            .on_close(move || flag.store(true, Ordering::SeqCst));
        assert!(ran.load(Ordering::SeqCst));
    }

    // ── register-then-revalidate, against a real store (AC 8, 10) ──

    use dom_render_compiler::auth::schema;
    use dom_render_compiler::auth::store::{
        create_session, grant_role_changed, revoke_all_sessions, revoke_role_changed,
        upsert_principal, ProviderProfile,
    };
    use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
    use dom_render_compiler::forge::LibSqlSubstrate;

    async fn store() -> Arc<dyn DataSubstrate> {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let tables = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
        bootstrap_schema(&db, &tables).await.unwrap();
        Arc::new(db)
    }

    /// Resolve a fresh session as the server would: principal + epoch + expiry.
    async fn signed_in(db: &dyn DataSubstrate, who: &str) -> (Identity, PrincipalId) {
        let principal = upsert_principal(
            db,
            "password",
            who,
            &ProviderProfile::default(),
            crate::auth::now_ms(),
        )
        .await
        .unwrap();
        let token = create_session(
            db,
            &principal.id,
            "password",
            crate::auth::now_ms(),
            86_400_000,
        )
        .await
        .unwrap();
        let resolved = store::resolve_session(db, &token, crate::auth::now_ms())
            .await
            .unwrap();
        let principal_resolved = resolved.principal().unwrap().clone();
        (
            Identity::Authenticated {
                principal: Box::new(principal_resolved),
                token: token.hash(),
            },
            principal.id,
        )
    }

    /// AC 8 · a role revoke committed between registration and the read
    /// closes the open. Mutation "drop the re-validation query" fails this.
    #[tokio::test]
    async fn a_revoke_in_the_registration_gap_closes_the_open() {
        let db = store().await;
        let (identity, id) = signed_in(db.as_ref(), "ada").await;
        grant_role_changed(db.as_ref(), &id, "payroll", None, 0)
            .await
            .unwrap();
        // Resolve after the grant, so the admitted epoch is 1.
        let (identity, _) = match identity {
            Identity::Authenticated {
                mut principal,
                token,
            } => {
                principal.authority_epoch = 1;
                (Identity::Authenticated { principal, token }, ())
            }
            other => (other, ()),
        };
        let registry = Arc::new(LiveRegistry::new());
        let pause = registry.pause_at(Checkpoint::BeforeRevalidate);
        let handle = registry.register(LiveKind::Sse, &identity);
        let entry = handle.entry().clone();
        let opening = {
            let registry = registry.clone();
            let db = db.clone();
            tokio::spawn(async move { registry.revalidate(&entry, Some(db.as_ref())).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), pause.reached.notified())
            .await
            .expect("the open never reached its checkpoint");
        let epoch = revoke_role_changed(db.as_ref(), &id, "payroll")
            .await
            .unwrap()
            .unwrap();
        // The in-process scan sees the Opening entry too — but even without
        // it, the read below must close it. Prove the read alone suffices:
        let _ = epoch;
        pause.release.notify_one();
        assert!(!opening.await.unwrap(), "a stale open was promoted");
        assert!(handle.entry().is_closed());
    }

    /// AC 8, the other direction · logout-all commits, a fresh login opens and
    /// re-validates, and only then does the late scan run — closing exactly
    /// the deleted set. The new connection survives. Mutation "close by
    /// principal" fails this.
    #[tokio::test]
    async fn a_late_logout_all_scan_spares_a_session_minted_after_it() {
        let db = store().await;
        let (old_identity, id) = signed_in(db.as_ref(), "ada").await;
        let registry = Arc::new(LiveRegistry::new());
        let old = registry.register(LiveKind::Phosphor, &old_identity);
        assert!(registry.revalidate(old.entry(), Some(db.as_ref())).await);

        let deleted = revoke_all_sessions(db.as_ref(), &id).await.unwrap();
        // …scan delayed. A fresh login lands meanwhile:
        let token = create_session(
            db.as_ref(),
            &id,
            "password",
            crate::auth::now_ms(),
            86_400_000,
        )
        .await
        .unwrap();
        let resolved = store::resolve_session(db.as_ref(), &token, crate::auth::now_ms())
            .await
            .unwrap();
        let fresh_identity = Identity::Authenticated {
            principal: Box::new(resolved.principal().unwrap().clone()),
            token: token.hash(),
        };
        let fresh = registry.register(LiveKind::Phosphor, &fresh_identity);
        assert!(registry.revalidate(fresh.entry(), Some(db.as_ref())).await);

        assert_eq!(registry.close_tokens(&deleted), 1);
        assert!(old.entry().is_closed());
        assert!(
            fresh.entry().admits(),
            "the late scan closed a session minted after the commit"
        );
    }

    /// AC 8 lifecycle · close lands after re-validation said yes, before the
    /// promote: the CAS fails and the entry stays closed.
    #[tokio::test]
    async fn a_close_before_promote_wins() {
        let db = store().await;
        let (identity, _) = signed_in(db.as_ref(), "ada").await;
        let registry = Arc::new(LiveRegistry::new());
        let pause = registry.pause_at(Checkpoint::BeforePromote);
        let handle = registry.register(LiveKind::WebTransport, &identity);
        let entry = handle.entry().clone();
        let opening = {
            let registry = registry.clone();
            let db = db.clone();
            tokio::spawn(async move { registry.revalidate(&entry, Some(db.as_ref())).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), pause.reached.notified())
            .await
            .expect("the open never reached its checkpoint");
        handle.entry().close();
        pause.release.notify_one();
        assert!(!opening.await.unwrap());
        assert_eq!(handle.entry().state(), LiveState::Closed);
    }

    /// AC 10 · a failing re-validation query fails the open closed.
    #[tokio::test]
    async fn a_failed_revalidation_fails_closed() {
        let db = store().await;
        let (identity, _) = signed_in(db.as_ref(), "ada").await;
        db.execute(&format!("DROP TABLE {}", schema::AUTHORITY), &[])
            .await
            .unwrap();
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register(LiveKind::Sse, &identity);
        assert!(!registry.revalidate(handle.entry(), Some(db.as_ref())).await);
        assert!(handle.entry().is_closed());
        // …and with no substrate at all.
        let other = registry.register(LiveKind::Sse, &identity);
        assert!(!registry.revalidate(other.entry(), None).await);
    }

    /// Anonymous entries skip re-validation and are never matched (AC 10a, unit).
    #[tokio::test]
    async fn anonymous_entries_go_live_without_a_query() {
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register(LiveKind::Sse, &Identity::Anonymous);
        assert!(registry.revalidate(handle.entry(), None).await);
        assert_eq!(registry.close_expired(i64::MAX), 0);
        assert_eq!(registry.close_principal_below("u_anyone", i64::MAX), 0);
        assert!(handle.entry().admits());
    }

    /// AC 10 · a failing sweep query keeps the watermark, still closes expired
    /// entries, and the next good tick processes every change.
    #[tokio::test]
    async fn a_failed_sweep_keeps_the_watermark_and_still_expires() {
        let db = store().await;
        let (identity, id) = signed_in(db.as_ref(), "ada").await;
        let registry = Arc::new(LiveRegistry::new());
        let stale = registry.register(LiveKind::Sse, &identity);
        assert!(registry.revalidate(stale.entry(), Some(db.as_ref())).await);
        let expiring = registry.register(LiveKind::Sse, &authed("u_x", "tx", 0, 1));
        expiring.entry().promote();

        grant_role_changed(db.as_ref(), &id, "payroll", None, 0)
            .await
            .unwrap();
        db.execute(
            &format!("ALTER TABLE {} RENAME TO hidden", schema::AUTHORITY),
            &[],
        )
        .await
        .unwrap();
        let (watermark, outcome) = registry.sweep_tick(db.as_ref(), 0).await;
        assert!(outcome.is_err());
        assert_eq!(watermark, 0, "a failed tick advanced the watermark");
        assert!(
            expiring.entry().is_closed(),
            "expiry must not depend on the authority query"
        );
        assert!(stale.entry().admits());

        db.execute(
            &format!("ALTER TABLE hidden RENAME TO {}", schema::AUTHORITY),
            &[],
        )
        .await
        .unwrap();
        let (watermark, outcome) = registry.sweep_tick(db.as_ref(), watermark).await;
        assert!(outcome.is_ok());
        assert_eq!(watermark, 1);
        assert!(
            stale.entry().is_closed(),
            "the change was skipped after the failure"
        );
    }

    /// Register-before-resolve, fast path: nothing raced, so admission needs no
    /// query at all — proved by admitting with no substrate.
    #[tokio::test]
    async fn admit_with_no_race_needs_no_query() {
        let db = store().await;
        let (identity, _) = signed_in(db.as_ref(), "ada").await;
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register_presented(LiveKind::Sse, identity.token().cloned());
        assert_eq!(
            registry.admit(&handle, &identity, None).await,
            Admission::Fast
        );
        assert!(handle.entry().admits());
        let anon = registry.register_presented(LiveKind::Sse, None);
        assert_eq!(
            registry.admit(&anon, &Identity::Anonymous, None).await,
            Admission::Fast
        );
    }

    /// The race the generation fence exists for: the request's read happened,
    /// then a role change committed and scanned **before** the entry published
    /// its principal (so the scan could not find it). Admission must notice the
    /// moved generation, re-check with the query, and close. Mutation "skip the
    /// generation check" admits the stale open.
    #[tokio::test]
    async fn a_role_change_between_read_and_publish_is_caught() {
        let db = store().await;
        let (_, id) = signed_in(db.as_ref(), "ada").await;
        grant_role_changed(db.as_ref(), &id, "payroll", None, 0)
            .await
            .unwrap();
        let token = create_session(
            db.as_ref(),
            &id,
            "password",
            crate::auth::now_ms(),
            86_400_000,
        )
        .await
        .unwrap();
        let registry = Arc::new(LiveRegistry::new());
        // 1. register under the presented cookie
        let handle = registry.register_presented(LiveKind::Sse, Some(token.hash()));
        // 2. the request's read (epoch 1, payroll held)
        let resolved = store::resolve_session(db.as_ref(), &token, crate::auth::now_ms())
            .await
            .unwrap();
        let identity = Identity::Authenticated {
            principal: Box::new(resolved.principal().unwrap().clone()),
            token: token.hash(),
        };
        // 3. a role change commits and scans before publish — finds nothing
        let epoch = revoke_role_changed(db.as_ref(), &id, "payroll")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(registry.close_principal_below(id.as_str(), epoch), 0);
        // 4. admission must not go live on the stale read
        assert_eq!(
            registry.admit(&handle, &identity, Some(db.as_ref())).await,
            Admission::Refused
        );
        assert!(handle.entry().is_closed());
    }

    /// A session end during the same gap needs no fence: the entry is indexed
    /// by token from registration.
    #[tokio::test]
    async fn a_logout_between_registration_and_admit_closes_by_token() {
        let db = store().await;
        let (identity, _) = signed_in(db.as_ref(), "ada").await;
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register_presented(LiveKind::Phosphor, identity.token().cloned());
        assert_eq!(
            registry.close_tokens(&[identity.token().unwrap().clone()]),
            1
        );
        assert_eq!(
            registry.admit(&handle, &identity, None).await,
            Admission::Refused
        );
    }

    /// Indexes stay exact as entries come and go.
    #[test]
    fn indexes_are_cleaned_on_drop() {
        let registry = Arc::new(LiveRegistry::new());
        let a = registry.register(LiveKind::Sse, &authed("u_a", "t", 0, 10));
        let b = registry.register(LiveKind::Sse, &authed("u_a", "t", 0, 10));
        assert_eq!(registry.by_token.len(), 1);
        drop(a);
        assert_eq!(registry.by_token.len(), 1, "b still presents t");
        drop(b);
        assert!(registry.by_token.is_empty() && registry.by_principal.is_empty());
        assert_eq!(registry.with_token.load(Ordering::Relaxed), 0);
    }

    /// Cost probe (not a gate): `cargo test --release -p albedo-server --lib
    /// live_authority::tests::cost_probe -- --ignored --nocapture`.
    /// Opens = what one live open pays (register + admit); closes = one
    /// logout's scan; both at two background sizes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn cost_probe() {
        let dir = tempfile::tempdir().unwrap();
        let db = LibSqlSubstrate::open_local(&dir.path().join("forge.db"))
            .await
            .unwrap();
        let tables = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
        bootstrap_schema(&db, &tables).await.unwrap();
        let db: Arc<dyn DataSubstrate> = Arc::new(db);
        let (identity, _) = signed_in(db.as_ref(), "ada").await;

        for background in [100usize, 10_000] {
            let registry = Arc::new(LiveRegistry::new());
            let keep: Vec<_> = (0..background)
                .map(|i| {
                    registry.register(
                        LiveKind::Sse,
                        &authed(&format!("u_bg{i}"), &format!("bg{i}"), 0, i64::MAX - 1),
                    )
                })
                .collect();
            let opens = 2_000;
            let started = std::time::Instant::now();
            for _ in 0..opens {
                let handle = registry.register(LiveKind::Sse, &identity);
                assert!(registry.revalidate(handle.entry(), Some(db.as_ref())).await);
            }
            let per_open = started.elapsed() / opens;
            let started = std::time::Instant::now();
            for _ in 0..opens {
                let handle = registry.register_presented(LiveKind::Sse, identity.token().cloned());
                assert!(registry
                    .admit(&handle, &identity, Some(db.as_ref()))
                    .await
                    .admitted());
            }
            let per_open_fast = started.elapsed() / opens;
            let closes = 2_000;
            let started = std::time::Instant::now();
            for i in 0..closes {
                registry.close_tokens(&[TokenHash::of(&format!("absent{i}"))]);
            }
            let per_logout = started.elapsed() / closes;
            let started = std::time::Instant::now();
            for i in 0..closes {
                registry.close_principal_below(&format!("u_absent{i}"), 1);
            }
            let per_role_change = started.elapsed() / closes;
            eprintln!(
"COST background={background:>6}  open(query)={per_open:?}  open(fast)={per_open_fast:?}  logout-scan={per_logout:?}  role-change-scan={per_role_change:?}"
            );
            drop(keep);
        }
    }

    #[test]
    fn dropping_the_handle_closes_and_unregisters() {
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register(LiveKind::Sse, &authed("u_a", "t", 0, 10));
        let entry = handle.entry().clone();
        drop(handle);
        assert!(entry.is_closed());
        assert!(registry.is_empty());
        assert!(registry.expiries.lock().unwrap().is_empty());
    }
}
