//! Type Resolver — resolves a meter's declaration from `types-registry`.
//!
//! Resolution sits on the ingestion hot path, and `types-registry` publishes
//! no latency obligation of its own, so a per-entry registry call would make
//! this gear's ingestion NFRs contingent on a second gear's availability and
//! latency. A local cache of resolved declarations keeps those obligations
//! self-contained.
//!
//! Fold, canonical unit and metadata surface are immutable for a type's life,
//! so a cached entry cannot silently change meaning — only additions and
//! withdrawals of types propagate. The TTL is nonetheless load-bearing
//! rather than a convenience: `types-registry` is moving to a model where a
//! major-only GTS identifier names a mutable entity, and a no-expiry cache
//! would not survive that.

mod declaration;
mod metadata;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;
use crate::domain::ports::declarations::DeclarationSource;

pub use declaration::ResolvedDeclaration;
pub use metadata::CompiledMetadataSchema;

/// Cache policy for the [`TypeResolver`].
///
/// Wired from gear configuration in a later task; a plain constructor
/// argument here so the cache policy itself can be exercised in isolation.
#[derive(Debug, Clone, Copy)]
pub struct TypeResolverConfig {
    /// How long a resolved declaration is served before it is treated as
    /// stale and refetched. Load-bearing, not a convenience: `types-registry`
    /// is moving to a model where a major-only GTS identifier names a
    /// mutable entity, so a cached declaration must eventually be checked
    /// against the registry again rather than trusted forever.
    pub ttl: Duration,
    /// Ceiling on the number of distinct meters held at once. Bounds
    /// *distinct meters*, not entries or requests — that count grows slowly
    /// (new meter types are declared far less often than usage records are
    /// ingested), so a small ceiling with simple eviction is sufficient; see
    /// [`TypeResolver::store`].
    pub capacity: usize,
}

/// One cached declaration plus the instant it was fetched.
struct CacheEntry {
    declaration: Arc<ResolvedDeclaration>,
    fetched_at: Instant,
}

/// Resolves a meter's `gts_type_id` to its declaration, fail-closed.
///
/// Serves a fresh cached entry directly, single-flights concurrent misses on
/// the same key so a cold key under load makes one registry call rather than
/// a burst, and — past the TTL — falls back to the stale entry when the
/// source is unavailable rather than blocking ingestion on a second gear's
/// availability. See [`Self::resolve`] for the full policy.
pub struct TypeResolver {
    source: Arc<dyn DeclarationSource>,
    config: TypeResolverConfig,
    entries: RwLock<HashMap<MeterTypeId, CacheEntry>>,
    /// One in-flight-fetch gate per key currently being populated. Collapses
    /// concurrent misses so a cold key under load does not fan a burst of
    /// identical reads at the registry. Entries are removed once the fetch
    /// they gate completes ([`Self::forget_gate`]), so this map tracks keys
    /// with a fetch in flight *right now*, not every meter the resolver has
    /// ever seen — its size does not accumulate over the resolver's
    /// lifetime the way an unbounded cache would.
    inflight: Mutex<HashMap<MeterTypeId, Arc<Mutex<()>>>>,
}

impl TypeResolver {
    /// Creates a resolver reading through to `source` on a cache miss.
    #[must_use]
    pub fn new(source: Arc<dyn DeclarationSource>, config: TypeResolverConfig) -> Self {
        Self {
            source,
            config,
            entries: RwLock::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Resolves `id` to its declaration.
    ///
    /// A fresh cached entry (within the TTL) is served directly. Past the
    /// TTL — or on a first request — the resolver refetches from `source`,
    /// single-flighted per key so concurrent callers on the same cold key
    /// collapse to one registry call rather than one each.
    ///
    /// A fetch that comes back unresolvable is handled according to what
    /// kind of unresolvable it is:
    /// - A definite not-found (or an incomplete declaration, which
    ///   [`DomainError::is_declaration_not_found`] also reports true for —
    ///   see that method's doc comment) is a conclusive fact, not a
    ///   possibly-stale read: it fails closed immediately and is **not**
    ///   cached, so a type declared moments later resolves on the very next
    ///   call rather than waiting out a negative TTL.
    /// - Any other failure (the registry is unreachable, times out, ...) is
    ///   treated as transient: a stale cached entry, if one exists, is
    ///   served instead, so a registry outage degrades the introduction of
    ///   *new* types rather than the ingestion of types already known. With
    ///   nothing cached to fall back on, it fails closed too.
    ///
    /// **The single-flight collapse applies to the success path only.** A
    /// failure is never cached (see above), so it is not shared with queued
    /// waiters either: each of them, in turn, finds the gate released with
    /// still nothing fresh to serve and reattempts its own fetch. That is by
    /// design, not a gap in the collapse — a not-found (or a corrected
    /// declaration) must resolve on the very next call rather than wait out
    /// a negative TTL, which only holds if failures are never cached for
    /// anyone to share. It does not reintroduce the stampede single-flight
    /// exists to prevent: waiters still retry **one at a time**, each behind
    /// the same gate in sequence, never concurrently.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when the declaration does not resolve and no
    /// cached entry can stand in for it. The gear never admits an entry
    /// whose type it could not validate against.
    pub async fn resolve(&self, id: &MeterTypeId) -> Result<Arc<ResolvedDeclaration>, DomainError> {
        if let Some(entry) = self.fresh_entry(id).await {
            return Ok(entry);
        }

        // Serialize the fetch per key: a waiter that queues up on the gate
        // observes whatever the fetch that held it produced (a populated
        // entry, or a definite failure) rather than issuing its own
        // redundant call.
        let gate = self.gate_for(id).await;
        let _held = gate.lock().await;

        let result = if let Some(entry) = self.fresh_entry(id).await {
            Ok(entry)
        } else {
            self.populate(id).await
        };

        // Release the gate for reuse before returning. Deliberately still
        // inside `_held`'s scope: a racing caller that misses this map entry
        // (post-removal) starts its own fetch attempt only after this one's
        // outcome — success or failure — is already visible via `entries`,
        // so it either hits the now-fresh entry or correctly reproduces a
        // not-cached failure; it never duplicates a fetch that already
        // populated the cache.
        self.forget_gate(id).await;

        result
    }

    /// Fetches, parses and caches `id`'s declaration.
    ///
    /// Called only once the per-key gate is held and a second freshness
    /// check has confirmed there is still nothing fresh to serve, so at
    /// most one of these runs per key at a time.
    async fn populate(&self, id: &MeterTypeId) -> Result<Arc<ResolvedDeclaration>, DomainError> {
        match self.source.fetch(id).await {
            Ok(schema) => match ResolvedDeclaration::from_schema(id.clone(), &schema) {
                Ok(declaration) => {
                    let declaration = Arc::new(declaration);
                    self.store(id.clone(), Arc::clone(&declaration)).await;
                    Ok(declaration)
                }
                // A schema that fetched fine but does not carry what a meter
                // needs (a missing trait, an unserved fold) is exactly as
                // unresolvable as a genuine not-found from the caller's
                // perspective — `ResolvedDeclaration::from_schema` reports it
                // through the same `DeclarationNotFound` variant
                // `is_declaration_not_found` tests for — so it takes the
                // same fail-closed, not-cached path: a corrected declaration
                // must resolve on the next call, not wait out a negative
                // TTL, and a parse failure must never be cached as if it
                // were a resolved success.
                Err(e) => Err(e),
            },
            Err(e) if e.is_declaration_not_found() => Err(e),
            Err(e) => match self.stale_entry(id).await {
                Some(stale) => {
                    tracing::warn!(
                        gts_type_id = %id,
                        error = %e,
                        "serving a stale declaration: types-registry is unavailable"
                    );
                    Ok(stale)
                }
                None => Err(e),
            },
        }
    }

    /// A cached entry within the TTL.
    async fn fresh_entry(&self, id: &MeterTypeId) -> Option<Arc<ResolvedDeclaration>> {
        let entries = self.entries.read().await;
        entries.get(id).and_then(|e| {
            (e.fetched_at.elapsed() < self.config.ttl).then(|| Arc::clone(&e.declaration))
        })
    }

    /// A cached entry regardless of age, for the registry-unavailable
    /// fallback.
    async fn stale_entry(&self, id: &MeterTypeId) -> Option<Arc<ResolvedDeclaration>> {
        let entries = self.entries.read().await;
        entries.get(id).map(|e| Arc::clone(&e.declaration))
    }

    /// Returns the single-flight gate for `id`, creating one if this is the
    /// first concurrent miss on this key.
    async fn gate_for(&self, id: &MeterTypeId) -> Arc<Mutex<()>> {
        let mut inflight = self.inflight.lock().await;
        Arc::clone(
            inflight
                .entry(id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Drops this resolver's map entry for `id`'s gate once its fetch has
    /// completed. A waiter that already holds its own clone of the gate is
    /// unaffected — the `Arc` keeps the mutex alive independently of the
    /// map — this only stops the map growing with every meter ever
    /// resolved instead of just those with a fetch in flight right now.
    async fn forget_gate(&self, id: &MeterTypeId) {
        self.inflight.lock().await.remove(id);
    }

    /// Inserts (or refreshes) `id`'s cached declaration.
    async fn store(&self, id: MeterTypeId, declaration: Arc<ResolvedDeclaration>) {
        let mut entries = self.entries.write().await;

        // Capacity bounds distinct meters, which grows slowly, so eviction
        // is a rare event on a small map: a linear scan for the oldest
        // fetch is reasonable here and avoids pulling in an LRU crate for a
        // cache this shape. Refreshing an existing key never evicts —
        // `entries.len()` does not grow when a key we already hold is
        // simply updated.
        if entries.len() >= self.config.capacity
            && !entries.contains_key(&id)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.fetched_at)
                .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest);
        }

        entries.insert(
            id,
            CacheEntry {
                declaration,
                fetched_at: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "resolver_tests.rs"]
mod resolver_tests;
