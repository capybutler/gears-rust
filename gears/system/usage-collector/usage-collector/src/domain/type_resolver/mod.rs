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
//!
//! **A temporary bridge inflates this module's branch count.**
//! `types-registry` stores declarations in memory, so a restart forgets
//! every declaration registered at run time. `TypeResolver::rehydrate`
//! (private), reachable only through [`TypeResolver::with_rehydration`],
//! mirrors each declaration this resolver reads into a durable table and
//! restores a forgotten one from that table the next time the registry
//! answers a definite not-found. The whole bridge — the mirror and registrar
//! ports, `rehydrate` itself, and
//! `infra::declaration_mirror`'s table — is deleted in one commit when
//! `types-registry` gains persistent storage on this gear's resolution path.
//! [`TypeResolver::new`] needs no change on that day: it never names the
//! bridge, and production's one construction point
//! (`infra::types_registry_source::build_default_resolvers`) is the only
//! caller that would move.

// `rehydrate` is named in plain text rather than linked because it is
// private, and a public module doc linking to a private item is a rustdoc
// warning this gear otherwise avoids.

mod declaration;
mod metadata;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::declaration_mirror::{DeclarationMirror, NoopDeclarationMirror};

const MIRROR_WRITE_TIMEOUT: Duration = Duration::from_millis(50);
use crate::domain::ports::declarations::{
    DeclarationRegistrar, DeclarationSource, UnavailableDeclarationRegistrar,
};
use crate::domain::ports::metrics::{TypeResolutionOutcome, UsageCollectorMetrics};

pub use declaration::ResolvedDeclaration;
pub use metadata::CompiledMetadataSchema;

/// Cache policy for the [`TypeResolver`]. A plain constructor argument, so
/// the cache policy can be exercised in isolation.
#[derive(Debug, Clone, Copy)]
pub struct TypeResolverConfig {
    /// How long a resolved declaration is served before it is treated as
    /// stale and refetched. Load-bearing rather than a convenience: see the
    /// module doc on the mutable-entity model `types-registry` is moving to.
    pub ttl: Duration,
    /// Ceiling on the number of *distinct meters* held at once — not entries
    /// or requests. That count grows slowly, so a small ceiling with simple
    /// eviction is sufficient; see [`TypeResolver::store`].
    pub capacity: usize,
}

/// One cached declaration, the instant it was fetched, and the instant it was
/// last used. It carries no retention value, and could not without
/// [`ResolvedDeclaration`] changing first — see that type's own doc.
struct CacheEntry {
    declaration: Arc<ResolvedDeclaration>,
    /// When this entry was last fetched from `types-registry` (or restored).
    /// `uc_declaration_cache_age_seconds` is defined against exactly this
    /// field, which [`TypeResolver::publish_cache_gauges`] reads; a cache hit
    /// or a stale serve must never write it.
    fetched_at: Instant,
    /// When this entry was last served to any caller — a fetch, a cache hit
    /// or a stale serve all count. [`TypeResolver::store`]'s eviction scan
    /// reads this rather than `fetched_at`, because it evicts the least
    /// recently *resolved* declaration, not the oldest fetch.
    ///
    /// A `std::sync::Mutex` rather than a bare [`Instant`]:
    /// [`TypeResolver::fresh_entry`]'s cache-hit check takes only a READ
    /// guard on `entries`, so updating this needs interior mutability
    /// reachable through a shared reference. Concurrent hits on DIFFERENT
    /// keys never contend, which is the property that matters — taking a
    /// write lock on `entries` per hit would serialize every key's hits
    /// against every other's, which is the hot-path contention the
    /// read/write split exists to avoid.
    last_used: StdMutex<Instant>,
}

/// Which half of the mirror-and-restore algorithm to run. An input rather
/// than two functions, so one body (`TypeResolver::rehydrate`) carries both
/// halves.
enum RehydrationMode<'a> {
    /// The registry just returned `document`; write the row and swallow a
    /// failure.
    Mirror {
        document: &'a Value,
        type_uuid: Uuid,
    },
    /// The registry answered a definite not-found; read the row, register it
    /// back, and re-fetch.
    Restore,
}

/// Resolves a meter's `gts_type_id` to its declaration, fail-closed.
///
/// Serves a fresh cached entry directly, single-flights concurrent misses on
/// the same key so a cold key under load makes one registry call rather than
/// a burst, and — past the TTL — falls back to the stale entry when the
/// source is unavailable rather than blocking ingestion on a second gear's
/// availability. See [`Self::resolve`] for the full policy.
// @cpt-dod:cpt-cf-usage-collector-dod-type-resolver-component:p1
pub struct TypeResolver {
    source: Arc<dyn DeclarationSource>,
    /// The DESIGN §3.7 declaration mirror. Temporary, with the rest of the
    /// bridge — see the module doc and [`Self::rehydrate`].
    mirror: Arc<dyn DeclarationMirror>,
    /// Registers a mirrored document back to `types-registry` on a restore.
    /// Temporary, with the rest of the bridge — see the module doc and
    /// [`Self::rehydrate`].
    registrar: Arc<dyn DeclarationRegistrar>,
    config: TypeResolverConfig,
    metrics: Arc<dyn UsageCollectorMetrics>,
    entries: RwLock<HashMap<MeterTypeId, CacheEntry>>,
    /// One in-flight-fetch gate per key currently being populated, collapsing
    /// concurrent misses so a cold key under load does not fan a burst of
    /// identical reads at the registry. Entries are removed once the fetch
    /// they gate completes ([`Self::forget_gate`]), so this map tracks keys
    /// with a fetch in flight right now and does not accumulate over the
    /// resolver's lifetime.
    inflight: Mutex<HashMap<MeterTypeId, Arc<Mutex<()>>>>,
}

impl TypeResolver {
    /// Creates a resolver reading through to `source` on a cache miss, with
    /// **no declaration mirror and no restore path**.
    ///
    /// `metrics` records one `uc_type_resolution_total{result}` sample per
    /// [`Self::resolve`] call — see [`TypeResolutionOutcome`].
    ///
    /// Production uses [`Self::with_rehydration`]; this is the convenience
    /// form, as [`crate::domain::service::Service::new`] is to
    /// `Service::new_with_metrics`. See
    /// [`crate::domain::ports::declaration_mirror::NoopDeclarationMirror`] for
    /// why the defaulted mirror is a no-op rather than a failing double.
    #[must_use]
    pub fn new(
        source: Arc<dyn DeclarationSource>,
        config: TypeResolverConfig,
        metrics: Arc<dyn UsageCollectorMetrics>,
    ) -> Self {
        Self::with_rehydration(
            source,
            Arc::new(NoopDeclarationMirror),
            Arc::new(UnavailableDeclarationRegistrar),
            config,
            metrics,
        )
    }

    /// Creates a resolver with the mirror and restore path wired.
    ///
    /// **Temporary**, with the bridge: when the mirror is retired, this
    /// constructor and the `mirror` / `registrar` fields go with it and
    /// [`Self::new`] becomes the only one.
    ///
    /// `entries` starts as an empty map built fresh on every call — no durable
    /// state, no warm-up step — and [`Self::new`] delegates here, so this is
    /// the single cold-start site.
    // @cpt-algo:cpt-cf-usage-collector-algo-maintain-declaration-cache:p1
    #[must_use]
    pub fn with_rehydration(
        source: Arc<dyn DeclarationSource>,
        mirror: Arc<dyn DeclarationMirror>,
        registrar: Arc<dyn DeclarationRegistrar>,
        config: TypeResolverConfig,
        metrics: Arc<dyn UsageCollectorMetrics>,
    ) -> Self {
        Self {
            source,
            mirror,
            registrar,
            config,
            metrics,
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
    /// failure is never cached, so it is not shared with queued waiters
    /// either: each in turn finds the gate released with nothing fresh to
    /// serve and reattempts its own fetch. That is by design — a not-found or
    /// a corrected declaration must resolve on the very next call — and it
    /// does not reintroduce the stampede, since waiters retry **one at a
    /// time**, each behind the same gate in sequence.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when the declaration does not resolve and no
    /// cached entry can stand in for it. The gear never admits an entry
    /// whose type it could not validate against.
    // @cpt-dod:cpt-cf-usage-collector-dod-fail-closed-resolution:p1
    // @cpt-dod:cpt-cf-usage-collector-dod-declaration-cache:p1
    pub async fn resolve(&self, id: &MeterTypeId) -> Result<Arc<ResolvedDeclaration>, DomainError> {
        // `cache_hit` and `cache_miss` on
        // `uc_type_resolution_duration_seconds` are timed from DIFFERENT
        // origins, deliberately. `start` is taken at this function's entry and
        // feeds both `CacheHit` sites (the immediate one below, and the
        // post-gate one), so the second INCLUDES however long this call waited
        // behind the gate — the wait is what that caller actually experienced.
        // `populate`'s `CacheMiss` sample is timed from its own `Instant`,
        // taken once the gate is held, so it EXCLUDES the wait.
        //
        // Consequence: on a cold start with N concurrent resolutions of one
        // meter, one sample lands on `cache_miss` (roughly the fetch cost) and
        // the other N-1 land on `cache_hit` carrying roughly the fetch cost
        // too, not the near-zero figure "cache hit" suggests. A restore
        // produces the same shape with no `cache_miss` sample at all, since
        // `Restored` is kept out of this histogram entirely.
        let start = std::time::Instant::now();
        if let Some(entry) = self.fresh_entry(id).await {
            self.metrics
                .record_type_resolution(TypeResolutionOutcome::CacheHit);
            self.metrics.observe_type_resolution_duration(
                TypeResolutionOutcome::CacheHit,
                start.elapsed().as_secs_f64(),
            );
            return Ok(entry);
        }

        // Serialize the fetch per key: a waiter that queues up on the gate
        // observes whatever the fetch that held it produced (a populated
        // entry, or a definite failure) rather than issuing its own
        // redundant call.
        let gate = self.gate_for(id).await;
        let _held = gate.lock().await;

        let result = if let Some(entry) = self.fresh_entry(id).await {
            // The post-gate `CacheHit` site: `start.elapsed()` includes the
            // `gate.lock().await` wait above, not just the freshness
            // re-check — see the comment at the top of this function.
            self.metrics
                .record_type_resolution(TypeResolutionOutcome::CacheHit);
            self.metrics.observe_type_resolution_duration(
                TypeResolutionOutcome::CacheHit,
                start.elapsed().as_secs_f64(),
            );
            Ok(entry)
        } else {
            self.populate(id).await
        };

        // Release the gate before returning, deliberately still inside
        // `_held`'s scope: a racing caller that misses this map entry starts
        // its own fetch only after this one's outcome is visible via
        // `entries`, so it either hits the now-fresh entry or reproduces a
        // not-cached failure, never duplicating a fetch that already
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
        // `populate`'s OWN `Instant`, taken after the per-key gate is held,
        // so the `CacheMiss` sample below EXCLUDES any gate wait — unlike
        // `resolve`'s post-gate `CacheHit` sample on the same histogram.
        let start = std::time::Instant::now();
        match self.source.fetch(id).await {
            Ok(schema) => match ResolvedDeclaration::from_schema(id.clone(), &schema) {
                Ok(declaration) => {
                    let declaration = Arc::new(declaration);
                    self.store(id.clone(), Arc::clone(&declaration)).await;
                    // Mirror mode always returns `Ok(None)`: a failed write is
                    // counted and logged inside `mirror_document` and never
                    // reaches here as an `Err`. Discarded with `drop(..)`
                    // rather than `.ok()`, which would suggest an `Err` this
                    // arm cannot produce; `let _ = ..` is rejected by this
                    // workspace's `clippy::let_underscore_must_use`.
                    drop(
                        self.rehydrate(
                            id,
                            RehydrationMode::Mirror {
                                document: &schema.raw_schema,
                                type_uuid: schema.type_uuid,
                            },
                        )
                        .await,
                    );
                    self.metrics
                        .record_type_resolution(TypeResolutionOutcome::CacheMiss);
                    self.metrics.observe_type_resolution_duration(
                        TypeResolutionOutcome::CacheMiss,
                        start.elapsed().as_secs_f64(),
                    );
                    Ok(declaration)
                }
                // A schema that fetched fine but does not carry what a meter
                // needs is as unresolvable as a genuine not-found, so it takes
                // the same fail-closed, not-cached path: a corrected
                // declaration must resolve on the next call rather than wait
                // out a negative TTL. `types-registry` answered fine here, so
                // this records `Unresolved`, not `RegistryError`.
                Err(e) => {
                    self.metrics
                        .record_type_resolution(TypeResolutionOutcome::Unresolved);
                    Err(e)
                }
            },
            // A definite not-found: the registry answered, the type is
            // simply not there. Restore mode gets one chance to put it back
            // from the declaration mirror before this is final.
            Err(e) if e.is_declaration_not_found() => {
                match self.rehydrate(id, RehydrationMode::Restore).await {
                    Ok(Some(declaration)) => {
                        self.store(id.clone(), Arc::clone(&declaration)).await;
                        self.metrics
                            .record_type_resolution(TypeResolutionOutcome::Restored);
                        // No duration sample: the histogram declares its
                        // `result` as (cache_hit, cache_miss) only, so a
                        // `restored` observation would ship an undeclared
                        // label value.
                        Ok(declaration)
                    }
                    // No row, an unreadable row, or a row naming another
                    // meter: the original not-found stands. `Unresolved`, not
                    // `RegistryError` — same reasoning as the
                    // incomplete-declaration arm above.
                    Ok(None) => {
                        self.metrics
                            .record_type_resolution(TypeResolutionOutcome::Unresolved);
                        Err(e)
                    }
                    // The registration or the re-fetch failed: fail-closed,
                    // counted as unresolved.
                    Err(restore_err) => {
                        self.metrics
                            .record_type_resolution(TypeResolutionOutcome::Unresolved);
                        Err(restore_err)
                    }
                }
            }
            Err(e) => self.handle_registry_error(id, e).await,
        }
    }

    /// `populate`'s final `else`: the fetch failed with something other than a
    /// definite not-found. Serves a stale entry if one exists, or fails closed
    /// with the registry-error population wrapped the way the restore path's
    /// failures are.
    ///
    /// Split out of `populate` to stay under this workspace's
    /// `clippy::cognitive_complexity` ceiling, the same reason `rehydrate`'s
    /// helpers were split out.
    ///
    /// A refresh that fails with a registry error keeps serving the existing
    /// cached copy unchanged: an aged declaration is due for refresh **rather
    /// than unusable**, and the "due for refresh" half — the decision that
    /// triggers this path — is [`Self::fresh_entry`]'s `.elapsed() < ttl`
    /// check. The `sample_cache_gauges` call here republishes the age, since a
    /// stale serve changes which entry is oldest without `store` running.
    // @cpt-algo:cpt-cf-usage-collector-algo-maintain-declaration-cache:p1
    async fn handle_registry_error(
        &self,
        id: &MeterTypeId,
        e: DomainError,
    ) -> Result<Arc<ResolvedDeclaration>, DomainError> {
        if let Some(stale) = self.stale_entry(id).await {
            tracing::warn!(
                gts_type_id = %id,
                error = %e,
                "serving a stale declaration: types-registry is unavailable"
            );
            // `store` is not called here — `fetched_at` is deliberately left
            // alone — but the age gauge is a function of elapsed time, so it
            // still has to be sampled again.
            //
            // That takes a second `entries` read lock. Not folded into
            // `stale_entry` (returning `fetched_at` alongside the declaration
            // so one guard serves both) because `resolver_tests.rs` calls
            // `stale_entry` directly as an `Option<..>`-shaped probe, and
            // widening its return type for gauge-only data would ripple into
            // those call sites for a lock only ever taken on the
            // registry-down degraded path.
            self.sample_cache_gauges().await;
            // A stale serve IS a resolution of this key, so it counts for
            // eviction recency even though, unlike a fetch, it must NOT reset
            // `fetched_at` — see `CacheEntry::last_used`.
            self.touch_last_used(id).await;
            self.metrics
                .record_type_resolution(TypeResolutionOutcome::ServedStale);
            Ok(stale)
        } else {
            // The fetch failed and nothing cached exists to fall back on: no
            // verdict on the type was ever reached, so `RegistryError`,
            // wrapped the way the restore path's failures are.
            self.metrics
                .record_type_resolution(TypeResolutionOutcome::RegistryError);
            Err(name_registry_failure(id, e))
        }
    }

    /// A cached entry within the TTL.
    ///
    /// Updates `last_used` on every hit: a cache hit is a resolution of this
    /// key just as a fetch is, and [`Self::store`]'s eviction scan must see it
    /// as recently used, not merely recently fetched. The update goes through
    /// the entry's own `last_used` mutex so this stays a READ-lock-only
    /// operation on `entries`.
    ///
    /// The `.elapsed() < ttl` comparison here is the "due for refresh"
    /// decision; the "rather than unusable" half is
    /// [`Self::handle_registry_error`]'s stale-serve arm. A plain cache hit
    /// republishes neither gauge.
    // @cpt-algo:cpt-cf-usage-collector-algo-maintain-declaration-cache:p1
    async fn fresh_entry(&self, id: &MeterTypeId) -> Option<Arc<ResolvedDeclaration>> {
        let entries = self.entries.read().await;
        entries.get(id).and_then(|e| {
            (e.fetched_at.elapsed() < self.config.ttl).then(|| {
                *e.last_used
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
                Arc::clone(&e.declaration)
            })
        })
    }

    /// A cached entry regardless of age, for the registry-unavailable
    /// fallback.
    ///
    /// Deliberately does **not** touch `last_used`, unlike
    /// [`Self::fresh_entry`]: `resolver_tests.rs` calls this directly as an
    /// `Option<..>`-shaped probe, and a side effect here would make that probe
    /// mutate the state it inspects. The `ServedStale` arm touches
    /// `last_used` itself, once it knows the entry is actually being served.
    async fn stale_entry(&self, id: &MeterTypeId) -> Option<Arc<ResolvedDeclaration>> {
        let entries = self.entries.read().await;
        entries.get(id).map(|e| Arc::clone(&e.declaration))
    }

    /// Recomputes and publishes `uc_resolved_types` /
    /// `uc_declaration_cache_age_seconds` (DESIGN §3.11.5) from `entries`'s
    /// current contents.
    ///
    /// **A second traversal, not a reuse of [`Self::store`]'s eviction scan,
    /// and over a DIFFERENT field.** That scan is pre-insert and conditional
    /// and answers "which entry should be evicted", reading `last_used`. This
    /// one is unconditional and answers "what is the oldest FETCH now", since
    /// the age gauge is defined against `fetched_at`.
    ///
    /// `entries` is never empty at either call site ([`Self::store`] and the
    /// `ServedStale` fallback via [`Self::sample_cache_gauges`]), so
    /// [`UsageCollectorMetrics::set_declaration_cache_age_seconds`]'s `None`
    /// arm is unreachable from this resolver; only
    /// `resolver_tests.rs`'s direct call over an empty map reaches it.
    fn publish_cache_gauges(&self, entries: &HashMap<MeterTypeId, CacheEntry>) {
        self.metrics
            .set_resolved_types(u64::try_from(entries.len()).unwrap_or(u64::MAX));
        let age = entries
            .values()
            .map(|e| e.fetched_at)
            .min()
            .map(|oldest| oldest.elapsed().as_secs());
        self.metrics.set_declaration_cache_age_seconds(age);
    }

    /// [`Self::publish_cache_gauges`] from a fresh read lock, for call
    /// sites that hold no write guard of their own to reuse the way
    /// [`Self::store`] does.
    ///
    /// Named `sample_*`, not `republish_*`: on the `ServedStale` fallback
    /// `entries` is unchanged (a stale serve never touches `fetched_at`) but
    /// time has moved on, so the *age* is new even though the data it is
    /// computed from is not. A periodic sampler is expected to call this on a
    /// tick — see the freeze property on
    /// [`UsageCollectorMetrics::set_declaration_cache_age_seconds`].
    async fn sample_cache_gauges(&self) {
        let entries = self.entries.read().await;
        self.publish_cache_gauges(&entries);
    }

    /// Marks `id`'s entry as used right now, for [`Self::store`]'s eviction
    /// scan.
    ///
    /// A READ-lock-only operation, like [`Self::fresh_entry`]'s own update:
    /// the mutation goes through `CacheEntry::last_used`'s interior mutex, not
    /// a write guard on `entries`. A no-op if `id` is no longer cached, which
    /// is harmless — the entry is already gone from the population the
    /// eviction policy chooses among.
    async fn touch_last_used(&self, id: &MeterTypeId) {
        if let Some(entry) = self.entries.read().await.get(id) {
            *entry
                .last_used
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        }
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
    ///
    /// The `entries.len() >= self.config.capacity` guard bounds the cache at
    /// `type_cache_capacity`, the eviction scan below is the
    /// least-recently-resolved policy, and `entries.insert` is both the
    /// cold-miss cache-and-return and the successful-refresh
    /// replace-and-reset-age in one call (refreshing an existing key is the
    /// same insert, not evicting itself). `fetched_at: now` records the
    /// instant of the registry read; [`Self::fresh_entry`] derives the age
    /// from it.
    // @cpt-algo:cpt-cf-usage-collector-algo-maintain-declaration-cache:p1
    async fn store(&self, id: MeterTypeId, declaration: Arc<ResolvedDeclaration>) {
        let mut entries = self.entries.write().await;

        // Capacity bounds distinct meters, which grows slowly, so eviction is
        // a rare event on a small map: a linear scan avoids pulling in an LRU
        // crate. Refreshing an existing key never evicts — `entries.len()`
        // does not grow when a key already held is updated.
        //
        // **Evicts the least recently RESOLVED entry.** The scan reads
        // `last_used`, not `fetched_at`: a cache hit and a stale serve both
        // update it, so a continuously referenced meter keeps winning the scan
        // regardless of when it was first fetched. `fetched_at` is untouched
        // here except on insert; see `CacheEntry`.
        if entries.len() >= self.config.capacity
            && !entries.contains_key(&id)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| {
                    *e.last_used
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                })
                .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest);
        }

        let now = Instant::now();
        entries.insert(
            id,
            CacheEntry {
                declaration,
                fetched_at: now,
                last_used: StdMutex::new(now),
            },
        );

        // Same write guard, no second acquisition: the eviction scan, this
        // insert and the size/age reads all run under the one
        // `entries.write().await` taken at the top of this method.
        self.publish_cache_gauges(&entries);
    }

    /// Runs the mirror-and-restore algorithm in `mode`.
    ///
    /// **Mirror mode** upserts the row and returns `Ok(None)` whatever
    /// happens: a failed mirror write does not reject the entry, so a failure
    /// is counted on `uc_declaration_mirror_write_failures_total`, logged, and
    /// swallowed.
    ///
    /// **Restore mode** requires that the caller already established a
    /// **definite** not-found (its only call site is `populate`'s
    /// `is_declaration_not_found` arm). It reads the row, refuses one whose
    /// document names a different meter, registers the document back, and then
    /// **re-fetches**: a locally rebuilt `GtsTypeSchema` is not possible for a
    /// derived type without its parent, and `effective_traits` walks the
    /// inheritance chain, so a reconstructed schema would drop an
    /// ancestor-declared trait.
    ///
    /// It applies **no lifecycle test**: while the restore is live a not-found
    /// answer is always loss, because the resolution surface carries no
    /// removal operation and no lifecycle status.
    ///
    /// **Mirror mode's write runs on the request path, inside the per-key
    /// single-flight gate, with a short local timeout** — see
    /// `Self::mirror_document`.
    ///
    /// # Errors
    ///
    /// In restore mode, [`DomainError`] when the registration is refused, or
    /// when the re-fetch fails or its schema does not parse into a usable
    /// declaration — every one fail-closed. An absent row, an unreadable row
    /// or a row naming a different meter answer `Ok(None)`: each is "nothing
    /// to restore" rather than a failure of the attempt.
    ///
    /// **Residual, named rather than closed:** the different-meter guard
    /// validates **identity only, never content**, so a row altered at the
    /// database level that still carries a matching `$id` is accepted and
    /// re-registered as this meter's declaration. The mirror is a trusted
    /// store and nothing here re-derives the document to check it against.
    //
    // @cpt-algo:cpt-cf-usage-collector-algo-mirror-and-restore:p1
    // @cpt-flow:cpt-cf-usage-collector-flow-survive-registry-loss:p1
    //
    // This dispatcher's tag covers the MODE gate alone (`RehydrationMode`,
    // matched below). `inst-restore-definite` and `inst-restore-no-lifecycle`
    // are realized by this method's CALLER and by the absence of any
    // lifecycle check in this mode's body. The steps WITHIN each mode are
    // realized by the private helpers below (`mirror_document`,
    // `restore_document`, `read_restorable_document`), split out to satisfy
    // `clippy::cognitive_complexity`; each carries the same tag scoped to the
    // steps it alone realizes, so a marker-grep lands on the right body.
    async fn rehydrate(
        &self,
        id: &MeterTypeId,
        mode: RehydrationMode<'_>,
    ) -> Result<Option<Arc<ResolvedDeclaration>>, DomainError> {
        match mode {
            RehydrationMode::Mirror {
                document,
                type_uuid,
            } => {
                self.mirror_document(id, type_uuid, document).await;
                Ok(None)
            }
            RehydrationMode::Restore => self.restore_document(id).await,
        }
    }

    /// Mirror mode's body. See [`Self::rehydrate`]'s doc for the mode gate
    /// this helper runs under.
    ///
    /// This write runs on the request path, inside the per-key single-flight
    /// gate, so it is locally timed out. Erroring and timed-out writes are
    /// counted, logged, swallowed, and the entry is still served.
    ///
    /// **The warm path is unaffected:** a `fresh_entry` hit returns before
    /// `populate` is reached, so a saturated mirror database costs a cold or
    /// TTL-expired resolution, never a cache hit.
    //
    // @cpt-algo:cpt-cf-usage-collector-algo-mirror-and-restore:p1
    //
    // Realizes `inst-mirror-write`, `inst-mirror-catch`, `inst-mirror-swallow`
    // and `inst-mirror-return` in full; the row write delegates to
    // `DeclarationMirror::upsert`, whose adapter carries a marker for that one
    // step. The literal `Ok(None)` that is the "RETURN success" sits in
    // `rehydrate`'s `Mirror` arm, not this body, which returns `()`.
    async fn mirror_document(&self, id: &MeterTypeId, type_uuid: Uuid, document: &Value) {
        let result = tokio::time::timeout(
            MIRROR_WRITE_TIMEOUT,
            self.mirror.upsert(id, type_uuid, document),
        )
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // Counted and swallowed, never propagated. The meter's identity
                // rides this log because the counter is unlabelled.
                tracing::warn!(
                    gts_type_id = %id,
                    error = %e,
                    "declaration mirror write failed: this meter will not be \
                     restorable after a types-registry restart"
                );
                self.metrics.record_declaration_mirror_write_failure();
            }
            Err(_) => {
                tracing::warn!(
                    gts_type_id = %id,
                    timeout_ms = MIRROR_WRITE_TIMEOUT.as_millis(),
                    "declaration mirror write timed out: this meter will not be \
                     restorable after a types-registry restart"
                );
                self.metrics.record_declaration_mirror_write_failure();
            }
        }
    }

    /// Restore mode's body, past the row read. See [`Self::rehydrate`]'s doc
    /// for the mode gate this helper runs under, and
    /// [`Self::read_restorable_document`] for the row-read half.
    //
    // @cpt-algo:cpt-cf-usage-collector-algo-mirror-and-restore:p1
    //
    // Realizes `inst-restore-register`, `inst-restore-catch`,
    // `inst-restore-fail` and `inst-restore-return`. `inst-restore-read` and
    // its different-meter elaboration belong to `read_restorable_document`
    // below. The registration step delegates to
    // `DeclarationRegistrar::register`, whose adapter carries its own marker.
    async fn restore_document(
        &self,
        id: &MeterTypeId,
    ) -> Result<Option<Arc<ResolvedDeclaration>>, DomainError> {
        let Some(document) = self.read_restorable_document(id).await else {
            return Ok(None);
        };

        // Logged before propagating, and `name_registry_failure` wraps `id`
        // into the `TypesRegistryUnavailable` detail text, so the error
        // reaching the caller names the meter rather than only the registry's
        // own refusal reason.
        if let Err(e) = self.registrar.register(&document).await {
            tracing::warn!(
                gts_type_id = %id,
                error = %e,
                "restoring a mirrored declaration was refused by types-registry; \
                 failing closed"
            );
            return Err(name_registry_failure(id, e));
        }

        // Re-fetch rather than rebuild: see `Self::rehydrate`'s doc.
        //
        // This step depends on an invariant stated nowhere else:
        // `MeterTypeId::new` admits exactly base-plus-ONE derivation segment,
        // so a meter's only ancestor is the reserved base type, which reaches
        // `types-registry` through the link-time `toolkit-gts` inventory
        // rather than a run-time registration and therefore survives the
        // restart that lost this meter. The one document the restore
        // registers is thus enough for `effective_traits`' inheritance walk.
        // Relax `MeterTypeId` to deeper chains and that stops holding: every
        // restore of a meter with a run-time-registered INTERMEDIATE ancestor
        // fails closed here, with nothing in the mirror able to repair it —
        // it holds one row per resolved meter, never one for an ancestor the
        // gear never resolved in its own right.
        //
        // The re-fetch's failure is wrapped like the registration refusal
        // above, so a transient outage mid-restore still names the meter.
        let schema = self
            .source
            .fetch(id)
            .await
            .map_err(|e| name_registry_failure(id, e))?;
        let declaration = Arc::new(ResolvedDeclaration::from_schema(id.clone(), &schema)?);
        Ok(Some(declaration))
    }

    /// Reads `id`'s mirror row and validates it names `id` itself.
    ///
    /// `None` covers all three "no usable row" cases alike (absent,
    /// unreadable, wrong name) — each is "nothing to restore", not a failure
    /// of the restore attempt itself; see the `# Errors` section on
    /// [`Self::rehydrate`]'s own doc.
    //
    // @cpt-algo:cpt-cf-usage-collector-algo-mirror-and-restore:p1
    //
    // Realizes `inst-restore-read` via `DeclarationMirror::read`, whose
    // adapter carries its own marker, and that step's elaboration — the
    // different-meter guard, where a row naming another meter is equivalent
    // to no usable row. The literal `Ok(None)` sits in `restore_document`'s
    // `let Some(document) = ... else { return Ok(None); }`, not here: this
    // function answers the `Option` that guard matches on.
    async fn read_restorable_document(&self, id: &MeterTypeId) -> Option<Value> {
        let document = match self.mirror.read(id).await {
            Ok(Some(document)) => document,
            Ok(None) => return None,
            Err(e) => {
                // NOT counted on the write-failure counter: that instrument's
                // population is "resolved but not mirrored", and a read
                // failure may leave a perfectly good row behind. Treated as
                // "no row", so resolution fails closed.
                tracing::warn!(
                    gts_type_id = %id,
                    error = %e,
                    "declaration mirror read failed; treating as no row"
                );
                return None;
            }
        };

        if document_names(&document, id) {
            Some(document)
        } else {
            // **This log is the ONLY signal this condition produces.** The
            // resolution counts `Unresolved` here, exactly what "mirrored
            // nowhere" counts, so a permanently unrestorable meter is
            // telemetrically identical to one never mirrored at all; the
            // instrument list is closed, so separating them is a normative
            // edit. What is available instead is a greppable marker token
            // (`uc-mirror-identity-refusal`) and the identifier the STORED
            // document names beside the expected one.
            //
            // The ways the stored document can name nothing are reported
            // **separately**, because they have different remedies: a
            // non-object row was written by something other than this
            // adapter, while an object carrying no identifier field is a
            // declaration that really was shaped wrongly.
            tracing::warn!(
                gts_type_id = %id,
                mirrored_gts_type_id = match document.as_object() {
                    None => "<the stored document is not a JSON object>",
                    Some(obj) => first_present_id(obj).unwrap_or(
                        "<the stored document is an object, but carries none \
                         of the three GTS identifier fields>",
                    ),
                },
                "uc-mirror-identity-refusal: declaration mirror row's document \
                 names a different meter; refusing to restore"
            );
            None
        }
    }
}

/// Wraps `id` into a [`DomainError::TypesRegistryUnavailable`] failure's
/// detail text, leaving every other variant untouched.
///
/// `DomainError::TypesRegistryUnavailable(String)` carries only the registry's
/// own message, never the identifier being resolved, since the port it crosses
/// is not meter-scoped — and a fail-closed rejection MUST name the unresolved
/// identifier. Applied at every site in this module that raises or propagates
/// the variant. The incomplete-declaration and definite-not-found arms need no
/// wrap: they already build [`DomainError::DeclarationNotFound`] with `id` in
/// the variant's own field.
fn name_registry_failure(id: &MeterTypeId, err: DomainError) -> DomainError {
    match err {
        DomainError::TypesRegistryUnavailable(detail) => {
            DomainError::TypesRegistryUnavailable(format!("{id}: {detail}"))
        }
        other => other,
    }
}

/// Whether `document`'s own GTS identifier is `id`.
///
/// Mirrors the **default** field list `types-registry`'s own `extract_gts_id`
/// iterates: it returns on the **first present** field, and strips the
/// `gts://` URI prefix **only** when that field is `$id`. A later field is
/// never consulted once an earlier one is present, agreeing or not.
///
/// **It cannot track a deployment that overrides `entity_id_fields`**, which
/// is the *default* of a deployment configuration key rather than a constant
/// in `types-registry`'s source. The override shapes are not equally safe:
/// - A **narrowed** list (fewer fields, same order) is safe by accident: a
///   document this guard accepts on a field the deployment no longer reads
///   makes `register` fail closed, and `restore_document` propagates that.
/// - A **reordered** list is exactly the condition this guard exists to
///   catch, and the guard gets it wrong — on a multi-field document that
///   disagrees across fields, the deployment registers under *its* first
///   field while this accepts on *ours*, re-registering another meter's
///   declaration.
///
/// `entity_id_fields` is not on `types-registry`'s SDK surface, so this gear
/// cannot read the deployment's actual list; the residual is recorded rather
/// than closed.
///
/// **The first-present exactness is load-bearing.** Checking every field with
/// `.any(...)` is silent on a single-field document — the overwhelming case —
/// but on a multi-field document that disagrees across fields, `.any()`
/// accepts on a non-first match while the registry registers under the first
/// field's different identity, which is precisely the cross-meter
/// re-registration this guard bounds.
fn document_names(document: &Value, id: &MeterTypeId) -> bool {
    let Some(obj) = document.as_object() else {
        return false;
    };
    first_present_id(obj) == Some(id.as_str())
}

/// The identifier `types-registry`'s `extract_gts_id` would read out of
/// `obj` under its default `entity_id_fields`, or `None` where it carries
/// none of them.
///
/// Split out of [`document_names`] so the refusal log in
/// `TypeResolver::read_restorable_document` can name **which** meter the
/// stored document claims to be without a second copy of the field list and
/// its precedence rules. The non-object guard stays in [`document_names`],
/// because `resolver_tests` pins that early return directly.
fn first_present_id(obj: &serde_json::Map<String, Value>) -> Option<&str> {
    for field in ["$id", "gtsId", "id"] {
        if let Some(raw) = obj.get(field).and_then(Value::as_str) {
            return Some(if field == "$id" {
                raw.strip_prefix(toolkit_gts::GTS_ID_URI_PREFIX)
                    .unwrap_or(raw)
            } else {
                raw
            });
        }
    }
    None
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "resolver_tests.rs"]
mod resolver_tests;
