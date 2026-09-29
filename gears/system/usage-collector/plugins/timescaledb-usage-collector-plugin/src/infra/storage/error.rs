// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (hypertable
// time-series, `time_bucket` aggregation, keyset pagination — see DESIGN.md). Tenant
// isolation is enforced by hand via parameterized `tenant_id` predicates and an
// allowlisted-identifier query builder (DESIGN.md §Injection-Safe Query Translation),
// not SecureConn/AccessScope.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use usage_collector_sdk::UsageCollectorPluginError;

/// Name of the dedup UNIQUE declared in `migrations/0001_init.sql`, over the
/// 6-tuple `(tenant_id, gts_type_id, idempotency_key, window_start,
/// window_end, entry_type)`.
const DEDUP_UNIQUE: &str = "usage_records_dedup_uniq";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorClass {
    DedupUniqueViolation,
    Transient,
    Other,
}

/// `55P03 lock_not_available` is in the transient set deliberately, and it is
/// the one member that is not a connectivity or serialization fault.
///
/// Every request-path connection carries a fixed `lock_timeout`
/// ([`crate::infra::storage::pool`]), so a statement that waits too long on a
/// contended lock fails with `55P03` rather than pinning a pooled connection.
/// The wait ingest is *sized* for is the speculative tuple an in-flight insert
/// of the same dedup 6-tuple holds until it commits, inside the write
/// transaction. One statement outside that transaction can wait too:
/// [`super::type_key::TypeKeyCache::resolve`] assigns a type's partition key
/// with its own `INSERT ... ON CONFLICT`, in autocommit, so two first writes of
/// one *type* meet on that tuple - once per type per process rather than once
/// per write ([`crate::infra::storage::pool`]'s bound covers it too, which is
/// not the same as its being the lock that constant is sized for).
/// A wait that times out is an ordinary contention outcome rather than a
/// defect, self-healing on retry, whichever lock it was on — which is why this
/// predicate asks nothing about *which*, and why nothing here rests on a list
/// of them ([`crate::infra::storage::pool`] names the ones worth a reader's
/// attention). Left in
/// `Other` it maps to a non-retryable `Internal` and
/// `is_retryable_batch_error` refuses to re-run a batch that is idempotent by
/// construction.
///
/// The one coupling worth naming: `acquire_error_clears_readiness` routes a
/// backend-reported `Database` error through this same predicate, so a
/// transient SQLSTATE there clears the `ready` gauge. `55P03` cannot arrive on
/// that path: `pool.acquire()` usually hands back an idle pooled connection and
/// runs nothing at all, and when it does have to open a new one the session
/// GUCs travel as startup parameters ([`crate::infra::storage::pool`]) rather
/// than as `SET` statements — so no lock-taking statement runs either way and
/// the gauge is unaffected.
fn is_transient_sqlstate(code: &str) -> bool {
    code.starts_with("08")
        || matches!(
            code,
            "57P01" | "57P02" | "57P03" | "53300" | "40001" | "40P01" | "55P03"
        )
}

/// True when `actual` names the constraint `name`, allowing for `TimescaleDB`'s
/// chunk-local spellings.
///
/// A hypertable clones each constraint onto every chunk under a generated
/// name, and there are two shapes because there are two declaration sites:
/// `<chunk_id>_<name>` for a constraint declared in `CREATE TABLE`, and
/// `_hyper_<ht>_<chunk>_chunk_<name>` for a standalone `CREATE UNIQUE INDEX`.
/// Both end in `_<name>`, and `chunk_id` is a global sequence across every
/// hypertable in the database, so no fixed prefix can be hardcoded. Bare
/// equality still holds for CHECK constraints, which are not renamed, and for
/// non-hypertable tables such as `usage_type_key` and
/// `usage_feed_retention_marks`.
///
/// The `_` anchor over a plain `ends_with` costs nothing and stops an
/// unrelated name that merely ends in the same characters without a separator.
///
/// Suffix matching is safe for this schema because no name in
/// `migrations/0001_init.sql` is a suffix of any other — note in particular
/// that `usage_records_tenant_window_idx` is *not* a suffix of
/// `usage_records_tenant_type_window_idx`. One rule follows for future
/// migrations: Postgres truncates an identifier to its **first** 63 bytes, and
/// the chunk prefix is *prepended* — so it is the tail that gets cut, and a
/// long enough name loses the suffix this function matches on entirely. Keep
/// constraint names under ~45 characters, which leaves room for the longest
/// chunk prefix observed (`_hyper_<ht>_<chunk>_chunk_`).
fn is_constraint(actual: &str, name: &str) -> bool {
    actual == name || actual.strip_suffix(name).is_some_and(|p| p.ends_with('_'))
}

/// Name of the ledger's PRIMARY KEY as `PostgreSQL` derives it from
/// `PRIMARY KEY (id, window_end, type_key)` (`migrations/0001_init.sql`).
const LEDGER_PK: &str = "usage_records_pkey";

/// True when `err` is a unique violation on the ledger's PRIMARY KEY.
///
/// **On a faithfully derived `id` this denotes the same dedup identity
/// `DEDUP_UNIQUE` covers, and that is why the write path can resolve it rather
/// than fail.** That constant is named
/// in plain backticks rather than linked, because it is private and this item is
/// public. The entry `id` is a
/// `UUIDv5` over the same 6-tuple the dedup UNIQUE spans
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`), so two rows that
/// collide on the primary key collide on the dedup identity too —
/// the migration says as much where it keeps the two as "the same key". The
/// premise is the derivation's, not this predicate's: the plugin takes the `id`
/// as given (`crate::infra::storage::record_store`'s `entry_identity`), so a
/// gateway that derived one over some other six inputs can make the two
/// disagree, and then the resolution this licenses resolves against a row that
/// is not the submission's own identity.
///
/// **Under a faithful derivation it is reachable only under concurrency, and
/// only because an `ON CONFLICT`
/// arbiter names one index.** Both insert paths arbitrate on the dedup 6-tuple,
/// and `DO NOTHING` suppresses a conflict on *that* index alone; a conflict on
/// any other unique index is raised as an ordinary `23505`. A writer whose
/// arbiter pre-check finds nothing therefore speculatively inserts, and its
/// insert into the primary-key index can still meet a concurrent writer of the
/// same identity. Serially this cannot happen *of two faithful submissions*:
/// the pre-check sees the committed row and skips it without touching any
/// index. A mis-derived `id` over novel six inputs can reach it serially, which
/// `crate::infra::storage::record_store`'s `entry_identity` sets out.
///
/// This is deliberately **not** folded into [`classify_db`], which maps a
/// primary-key `23505` to [`DbErrorClass::Other`] and so to a non-retryable
/// `Internal`. That remains right for a primary-key violation nothing
/// intercepted: with the derivation correct it is unreachable outside this race,
/// so reaching the catch-all still means something is wrong. This predicate is
/// the write path declaring which primary-key violations it will attempt a
/// resolution for rather than surface as a defect, and the division of labour
/// is the point —
/// `error_tests::the_ledger_pk_is_recognised_under_every_chunk_local_spelling`
/// asserts both halves on one input.
#[must_use]
pub fn is_ledger_pk_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db)
        if db.code().as_deref() == Some("23505")
            && db.constraint().is_some_and(|c| is_constraint(c, LEDGER_PK)))
}

/// 23503 `foreign_key_violation` has no arm: the schema declares no foreign
/// key, so it is unreachable and falls to `Other`.
#[must_use]
pub fn classify_db(code: &str, constraint: Option<&str>) -> DbErrorClass {
    match code {
        // Match the dedup constraint by name. Any other unique constraint —
        // the records PK `(id, window_end, type_key)`, say — must fall through to
        // `Other`.
        //
        // [`DEDUP_UNIQUE`] is the dedup authority, but the ingest path reaches
        // it via `INSERT … ON CONFLICT … DO NOTHING`, which suppresses the
        // 23505 — so that arm is defensive: it only fires if a dedup-unique
        // violation ever surfaces as a raw error (e.g. a future write path that
        // bypasses `ON CONFLICT`), keeping it classified rather than `Other`.
        //
        // It is matched through [`is_constraint`], not by equality: on a
        // real hypertable it does not report its bare name.
        "23505" => match constraint {
            Some(c) if is_constraint(c, DEDUP_UNIQUE) => DbErrorClass::DedupUniqueViolation,
            _ => DbErrorClass::Other,
        },
        c if is_transient_sqlstate(c) => DbErrorClass::Transient,
        _ => DbErrorClass::Other,
    }
}

/// Whether a `pool.acquire()` failure should clear the `uc_timescaledb_ready`
/// gauge — i.e. whether it indicates lost backend *connectivity* rather than a
/// healthy-but-saturated pool. `live_connections` is the pool's current
/// established-connection count (`PgPool::size`) at the time of the failure.
///
/// `PoolTimedOut` is the crux: sqlx funnels *both* pool saturation (every
/// connection checked out, backend healthy) *and* connection-establishment
/// failure (backend unreachable) into this one variant after the acquire
/// timeout elapses, so the variant alone cannot tell them apart. Occupancy
/// resolves it: a healthy-but-saturated pool always still holds its established
/// connections (`live_connections > 0` — "all busy", not an outage), whereas an
/// unreachable backend cannot keep any (`live_connections == 0` — a real
/// outage). This stops the gauge flapping under load (the false-positive)
/// without losing connection-refused outage detection (the most
/// common manifestation, which arrives as `PoolTimedOut` with zero live
/// connections). Sustained saturation is covered separately by the
/// `pool.connections.active` vs `pool_size_max` SLO.
#[must_use]
pub fn acquire_error_clears_readiness(err: &sqlx::Error, live_connections: u32) -> bool {
    match err {
        // Ambiguous timeout: saturation iff the pool still holds connections.
        sqlx::Error::PoolTimedOut => live_connections == 0,
        // A fresh physical connection was refused/reset, or its TLS handshake
        // failed, or the pool has been torn down: genuine loss of connectivity.
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolClosed => true,
        // Backend-reported connection-class SQLSTATE (server shutdown, too many
        // connections, ...): treat exactly like the transient classification.
        sqlx::Error::Database(db) => is_transient_sqlstate(db.code().as_deref().unwrap_or("")),
        // Anything else (decode/protocol/config/query-shaped) is not an outage.
        _ => false,
    }
}

/// `(sqlstate, constraint)` if `err` is a DB error.
#[must_use]
pub fn db_code_and_constraint(err: &sqlx::Error) -> Option<(String, Option<String>)> {
    if let sqlx::Error::Database(db) = err {
        return Some((
            db.code()?.into_owned(),
            db.constraint().map(ToOwned::to_owned),
        ));
    }
    None
}

/// Catch-all mapping for non-classified sqlx errors (transient vs internal).
#[must_use]
pub fn map_sqlx_err(err: &sqlx::Error) -> UsageCollectorPluginError {
    if let sqlx::Error::Database(db) = err
        && classify_db(db.code().as_deref().unwrap_or(""), db.constraint())
            == DbErrorClass::Transient
    {
        return UsageCollectorPluginError::transient("transient database error");
    }
    // Connectivity-class transport faults are retryable. `Tls` is included to
    // stay consistent with `acquire_error_clears_readiness`, which already
    // treats a TLS failure as a connectivity outage — a query-time TLS blip
    // must lift to a retryable Transient, not a non-retryable Internal.
    if matches!(
        err,
        sqlx::Error::PoolTimedOut
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolClosed
    ) {
        return UsageCollectorPluginError::transient("database unavailable");
    }
    // Do NOT format the raw error into the user-facing detail: the SDK contract
    // (`UsageCollectorPluginError::Internal`) requires it to be DSN-free /
    // pre-redacted, and a `sqlx::Error` Display (e.g. `Configuration` / `Tls`
    // source chains) can carry connection-string fragments. Log the full error
    // for operators (logs/traces are the right home for diagnostics) and return
    // a fixed token.
    tracing::error!(error = %err, "unclassified backend error mapped to Internal");
    UsageCollectorPluginError::internal("database error")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
