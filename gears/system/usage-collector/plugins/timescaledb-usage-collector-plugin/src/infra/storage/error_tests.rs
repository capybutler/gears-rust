// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use super::*;

#[test]
fn unique_violation_on_dedup_is_dedup_conflict() {
    assert_eq!(
        classify_db("23505", Some("usage_records_dedup_uniq")),
        DbErrorClass::DedupUniqueViolation
    );
}
#[test]
fn unique_violation_on_unknown_constraint_is_other() {
    // A future second unique constraint (or a records PK collision) must not be
    // misclassified as the dedup-specific violation.
    assert_eq!(
        classify_db("23505", Some("usage_records_pkey")),
        DbErrorClass::Other
    );
}
#[test]
fn unique_violation_without_constraint_is_other() {
    assert_eq!(classify_db("23505", None), DbErrorClass::Other);
}

/// Both assertions are load-bearing, and the second is deliberately an input
/// the database will never produce — do not "tidy" it away.
///
/// The schema has no foreign key — the only one, `usage_records_gts_id_fk`,
/// went with the `usage_type_catalog` table — so 23503 is unreachable and must
/// classify as `Other`. This pins in code the claim `classify_db` otherwise
/// only makes in its doc.
///
/// The mutation it defends against is widening the dedup arm's guard from the
/// exact `"23505"` to the SQLSTATE *class*, `c if c.starts_with("23")`, which
/// would read a foreign-key violation as a dedup conflict. No other test in
/// this file is class-sensitive: `42601` is syntax-class and stays `Other`
/// under the widening.
///
/// Under that widening the *realistic* pairing — 23503 with the FK's own name —
/// still falls to the inner arm's `_` and stays `Other`, so it does not
/// discriminate on its own; pairing 23503 with the dedup constraint's name is
/// what forces the inner arm to fire. Together they say the load-bearing thing:
/// it is the *code* that makes something a dedup conflict, not the constraint
/// name riding along with it.
#[test]
fn fk_violation_is_other_because_the_schema_has_no_foreign_key() {
    assert_eq!(
        classify_db("23503", Some("usage_records_gts_id_fk")),
        DbErrorClass::Other,
        "the base migration creates no foreign key, so 23503 has no meaning to \
         classify and must not resurrect a dedicated class"
    );
    assert_eq!(
        classify_db("23503", Some("usage_records_dedup_uniq")),
        DbErrorClass::Other,
        "23503 shares its SQLSTATE class with the special-cased 23505, so a \
         guard widened to the class would misread this as a dedup conflict"
    );
}

#[test]
fn connection_class_is_transient() {
    assert_eq!(classify_db("08006", None), DbErrorClass::Transient);
    assert_eq!(classify_db("57P03", None), DbErrorClass::Transient);
}
#[test]
fn unknown_code_is_other() {
    assert_eq!(classify_db("42601", None), DbErrorClass::Other);
}

#[test]
fn pool_timed_out_while_saturated_does_not_clear_readiness() {
    // A saturated-but-healthy pool returns PoolTimedOut while still holding its
    // established connections. Clearing readiness here flaps the gauge and
    // raises a false `ready == 0` outage signal under load.
    assert!(!acquire_error_clears_readiness(
        &sqlx::Error::PoolTimedOut,
        8 // live connections (pool at capacity)
    ));
}

#[test]
fn pool_timed_out_with_no_live_connections_clears_readiness() {
    // The connection-refused / unreachable-backend case: sqlx surfaces it as a
    // PoolTimedOut after the acquire timeout, but the pool holds zero live
    // connections. This is a genuine outage and must clear readiness.
    assert!(acquire_error_clears_readiness(
        &sqlx::Error::PoolTimedOut,
        0
    ));
}

#[test]
fn connectivity_failures_clear_readiness() {
    use std::io;
    assert!(
        acquire_error_clears_readiness(
            &sqlx::Error::Io(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "connection refused"
            )),
            4
        ),
        "a refused/lost physical connection is a genuine connectivity outage"
    );
    assert!(
        acquire_error_clears_readiness(&sqlx::Error::PoolClosed, 4),
        "a closed pool means the backend is no longer serving connections"
    );
}

#[test]
fn non_connectivity_errors_do_not_clear_readiness() {
    // A query-shaped error reaching the acquire predicate (defensively) is not
    // an outage signal, regardless of pool occupancy.
    assert!(!acquire_error_clears_readiness(
        &sqlx::Error::RowNotFound,
        0
    ));
}

#[test]
fn tls_error_at_query_time_maps_to_transient() {
    // A TLS transport blip at query time is a connectivity-class failure:
    // `acquire_error_clears_readiness` already treats `Tls` as an outage (it
    // clears the `ready` gauge). The catch-all query-error mapping must agree
    // and classify it retryable (Transient), not a non-retryable Internal —
    // otherwise readiness and retry-ability disagree on the same fault.
    let err = sqlx::Error::Tls(Box::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "tls handshake failed",
    )));
    match map_sqlx_err(&err) {
        UsageCollectorPluginError::Transient { detail, .. } => {
            // Still DSN-free: a fixed token, never the raw sqlx Display.
            assert_eq!(detail, "database unavailable");
        }
        other => panic!("expected Transient for a TLS transport error, got {other:?}"),
    }
}

#[test]
fn internal_mapping_does_not_leak_raw_error_text() {
    // The SDK contract (UsageCollectorPluginError::Internal) requires the detail
    // to be DSN-free / pre-redacted. A `sqlx::Error` Display (Configuration/Tls
    // source chains) can carry connection-string fragments, so the catch-all
    // must not format the raw error into the user-facing detail.
    let mapped = map_sqlx_err(&sqlx::Error::RowNotFound);
    match mapped {
        UsageCollectorPluginError::Internal(detail) => {
            assert_eq!(
                detail, "database error",
                "Internal detail must be a fixed, DSN-free string"
            );
            assert!(
                !detail.contains("no rows"),
                "the raw sqlx::Error Display must not leak into the detail"
            );
        }
        other => panic!("expected Internal, got {other:?}"),
    }
}

// --- TimescaleDB chunk-local constraint spellings ---
//
// A hypertable clones each constraint onto every chunk under a generated name,
// so the bare name a migration declares is not what a violation reports. These
// inputs are the literal strings a live `timescale/timescaledb:latest-pg16`
// returned from `db.constraint()` against this crate's own
// `migrations/0001_init.sql`, so they pin the finding rather than a guess at
// it.

#[test]
fn a_chunk_local_dedup_constraint_is_still_the_dedup_violation() {
    // Declared inside `CREATE TABLE`, so TimescaleDB clones it as
    // `<chunk_id>_<name>`. `chunk_id` is a global sequence across every
    // hypertable in the database, so the prefix cannot be hardcoded.
    assert_eq!(
        classify_db("23505", Some("1_usage_records_dedup_uniq")),
        DbErrorClass::DedupUniqueViolation,
        "the chunk-local spelling is the only one a real hypertable reports, so \
         exact-name matching would leave this arm dead"
    );
    assert_eq!(
        classify_db("23505", Some("2_usage_records_dedup_uniq")),
        DbErrorClass::DedupUniqueViolation,
        "a second chunk carries a different numeric prefix"
    );
}

#[test]
fn a_chunk_local_one_invalidation_index_is_already_invalidated() {
    // Declared as a standalone `CREATE UNIQUE INDEX`, so Postgres clones it
    // onto the chunk as `_hyper_<ht>_<chunk>_chunk_<name>` — a different shape
    // from the in-table constraint above. A normalization that strips one
    // shape silently misses the other; a boundary-anchored suffix match takes
    // both.
    assert_eq!(
        classify_db(
            "23505",
            Some("_hyper_1_1_chunk_usage_records_one_invalidation_uniq")
        ),
        DbErrorClass::AlreadyInvalidated,
        "the at-most-one-invalidation index is the atomic enforcement, and this \
         is the spelling its violation actually arrives under"
    );
    assert_eq!(
        classify_db("23505", Some("usage_records_one_invalidation_uniq")),
        DbErrorClass::AlreadyInvalidated,
        "the bare name still matches, for a non-hypertable or a future rename"
    );
}

#[test]
fn a_name_that_merely_ends_in_a_constraint_name_is_not_that_constraint() {
    // The suffix match is anchored on the `_` separator every chunk-local
    // spelling ends its prefix with. Without that anchor an unrelated
    // constraint whose name happens to end in the same characters would be
    // misread as the dedup authority.
    assert_eq!(
        classify_db("23505", Some("tenantusage_records_dedup_uniq")),
        DbErrorClass::Other,
        "no `_` separator before the suffix, so this is a different constraint"
    );
}

#[test]
fn lock_not_available_is_transient_so_the_batch_retry_can_see_it() {
    // Every request-path connection sets `lock_timeout` (pool.rs), and the
    // ingest path now takes a per-scope row lock on `usage_acceptance_sequence`
    // plus the dedup tuple lock, so a 5s wait that times out is an ordinary
    // contention outcome rather than a defect. Classified `Other` it would map
    // to a non-retryable `Internal` and `is_retryable_batch_error` would refuse
    // to re-run an operation that is idempotent by construction.
    assert_eq!(classify_db("55P03", None), DbErrorClass::Transient);
}
