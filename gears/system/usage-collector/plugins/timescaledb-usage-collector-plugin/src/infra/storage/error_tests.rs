// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use super::*;

/// A `sqlx::Error::Database` carrying a chosen SQLSTATE and constraint name.
///
/// `PgDatabaseError` cannot be constructed outside `sqlx`, but
/// [`is_ledger_pk_violation`] reads `code()` and `constraint()` off the
/// `DatabaseError` **trait** rather than off the concrete driver type - so
/// implementing the trait is what puts the predicate itself under test.
///
/// It is worth the twenty lines. The only other oracle in reach is
/// [`classify_db`], and that one answers `Other` for the ledger PK *and* for
/// every other constraint name alike: a test routed through it would still pass
/// with [`LEDGER_PK`] misspelled, with [`is_constraint`]'s `_` anchor deleted,
/// or with the predicate hardwired to `false`. A check that cannot fail is
/// worse than a missing one.
#[derive(Debug)]
struct FakeDbError {
    code: &'static str,
    constraint: Option<&'static str>,
}

impl std::fmt::Display for FakeDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} on {}",
            self.code,
            self.constraint.unwrap_or("<no constraint>")
        )
    }
}

impl std::error::Error for FakeDbError {}

impl sqlx::error::DatabaseError for FakeDbError {
    fn message(&self) -> &'static str {
        "fixture database error"
    }
    fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(std::borrow::Cow::Borrowed(self.code))
    }
    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }
    fn constraint(&self) -> Option<&str> {
        self.constraint
    }
    fn kind(&self) -> sqlx::error::ErrorKind {
        sqlx::error::ErrorKind::UniqueViolation
    }
}

/// A driver error with `code` and `constraint`, in the shape the predicates read.
fn db_err(code: &'static str, constraint: Option<&'static str>) -> sqlx::Error {
    sqlx::Error::Database(Box::new(FakeDbError { code, constraint }))
}

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
    //
    // **This stays `Other` on purpose, and the write path is what reclassifies a
    // primary-key collision it is expecting** (`is_ledger_pk_violation`, whose
    // own oracle is `the_ledger_pk_is_recognised_under_every_chunk_local_spelling`
    // below, where both halves are asserted on one input). The division matters:
    // a PK `23505` that no insert path intercepted
    // is unreachable while the derived identity is correct, so the catch-all
    // reading it as a non-retryable `Internal` is the loud failure it should be.
    // Folding the PK into this arm would silence that.
    assert_eq!(
        classify_db("23505", Some("usage_records_pkey")),
        DbErrorClass::Other
    );
}

/// The write-path predicate for a PRIMARY KEY collision, which
/// `create_inner` resolves in place and `create_batch_inner` lifts to a
/// `Transient`.
///
/// Asserted on [`is_ledger_pk_violation`] itself, through [`FakeDbError`],
/// because the predicate is what the fix turns on and no other oracle in this
/// file can see it: [`classify_db`] answers `Other` for the ledger PK and for
/// every other constraint alike.
///
/// The negatives are the load-bearing half. Each one names a different way the
/// predicate could be wrong, and a predicate that answered `true` to all comers
/// would turn a genuine defect into a silent absorb - the write path resolves
/// what this returns `true` for.
#[test]
fn the_ledger_pk_is_recognised_under_every_chunk_local_spelling() {
    // The bare name, the `CREATE TABLE` chunk clone, and the standalone-index
    // chunk clone - the three shapes `is_constraint`'s doc enumerates.
    for spelling in [
        "usage_records_pkey",
        "1_usage_records_pkey",
        "_hyper_1_1_chunk_usage_records_pkey",
    ] {
        assert!(
            is_ledger_pk_violation(&db_err("23505", Some(spelling))),
            "`{spelling}` names the ledger PK and must be recognised as one"
        );
        // The division of labour, stated on the same input: what the write path
        // resolves, the classifier still reads as `Other`, so a PK collision no
        // insert path intercepted stays a non-retryable `Internal`.
        assert_eq!(
            classify_db("23505", Some(spelling)),
            DbErrorClass::Other,
            "`{spelling}` is the ledger PK, not the dedup constraint"
        );
    }

    // The near-miss the `_` anchor exists to reject: a different constraint that
    // merely ends in the same characters without a separator.
    assert!(
        !is_ledger_pk_violation(&db_err("23505", Some("tenantusage_records_pkey"))),
        "no `_` separator before the suffix, so this is a different constraint"
    );
    // A different constraint entirely. The dedup UNIQUE is the one the arbiter
    // already suppresses, and reading it as a PK collision would resolve a row
    // the write path never inserted.
    assert!(
        !is_ledger_pk_violation(&db_err("23505", Some("usage_records_dedup_uniq"))),
        "the dedup UNIQUE is not the ledger PK"
    );
    // The right constraint under the wrong SQLSTATE: 23514 is a CHECK
    // violation, which `usage_records_invalidation_pairing` can really raise.
    assert!(
        !is_ledger_pk_violation(&db_err("23514", Some("usage_records_pkey"))),
        "only a 23505 on that constraint is a unique violation on it"
    );
    // A `23505` the driver reported without a constraint name.
    assert!(
        !is_ledger_pk_violation(&db_err("23505", None)),
        "an unattributed unique violation names no constraint to match"
    );
    // And a non-database error, which the `matches!` arm must not admit.
    assert!(
        !is_ledger_pk_violation(&sqlx::Error::RowNotFound),
        "only a backend-reported error can be a constraint violation"
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
    // PostgreSQL 18 reports an `ON DELETE RESTRICT` refusal as `23001` where 17
    // reported `23503`. With no foreign key it is just as unreachable.
    assert_eq!(
        classify_db("23001", Some("usage_records_gts_id_fk")),
        DbErrorClass::Other,
        "23001 is PostgreSQL 18's RESTRICT refusal, and there is no RESTRICT \
         foreign key left to refuse"
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
    // ingest path waits on the dedup tuple lock when an insert of the same
    // 6-tuple is already in flight, so a 5s wait that times out is an ordinary
    // contention outcome rather than a defect. Classified `Other` it would map
    // to a non-retryable `Internal` and `is_retryable_batch_error` would refuse
    // to re-run an operation that is idempotent by construction.
    assert_eq!(classify_db("55P03", None), DbErrorClass::Transient);
}
