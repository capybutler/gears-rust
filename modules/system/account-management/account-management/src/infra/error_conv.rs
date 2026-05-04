//! Infrastructure-layer DB error classification helpers used by the
//! boundary mapping in [`crate::infra::canonical_mapping`].
//!
//! After the AIP-193 / `#[domain_model]` refactor:
//!
//! - `domain::error::DomainError` is pure (no `sea_orm` / `modkit_db`
//!   imports). The `Database(DbErr)` variant has been removed.
//! - Inside `with_serializable_retry` the raw `DbErr` is carried by
//!   the infra-internal `TxError::Db` enum (`infra/storage/repo_impl/
//!   helpers.rs`) so the retry helper can extract the `DbErr` for
//!   contention detection. Once the retry budget is exhausted, the
//!   surviving `DbErr` is translated to a typed `DomainError`
//!   (`Aborted` / `AlreadyExists` / `ServiceUnavailable` / `Internal`)
//!   by [`crate::infra::canonical_mapping::classify_db_err_to_domain`].
//!
//! What stays here is **just the typed predicates** the boundary mapping
//! relies on: backend-aware retryable-contention detection, connectivity
//! signals, and the redacted diagnostic helper that drops operator-
//! supplied text (DSN, env-var values) before logging.

use modkit_db::DbError;
use modkit_db::contention::is_retryable_contention;
use sea_orm::{DbBackend, DbErr};

/// Backend-agnostic adapter for AM's two supported engines (Postgres,
/// `SQLite`). Replaces the workspace-removed
/// `modkit_db::deadlock::is_serialization_failure`: the new
/// [`is_retryable_contention`] takes an explicit [`DbBackend`] to keep
/// `SQLSTATE` matching scoped, but the boundary classifier does not have
/// access to the live backend. AM forbids `MySQL` at the storage layer
/// (see `infra/storage/migrations/m0001_initial_schema.rs` — "`MySQL`
/// backend is not a supported AM backend"), so probing PG and `SQLite`
/// is sufficient and avoids false positives from the unsupported `MySQL`
/// branch.
pub(crate) fn is_serialization_failure(err: &DbErr) -> bool {
    is_retryable_contention(DbBackend::Postgres, err)
        || is_retryable_contention(DbBackend::Sqlite, err)
}

/// Returns `true` iff `err` is a typed database connectivity / outage
/// signal — pool acquire timeout, connection closed, connection-level
/// runtime error, or a raw `std::io::Error` surfaced through
/// [`DbError::Io`]. Used to route those failures to
/// [`modkit_canonical_errors::CanonicalError::ServiceUnavailable`]
/// (HTTP 503) rather than `Internal` (HTTP 500), so clients see a
/// "retry later, transient infra outage" status that matches reality.
///
/// Classification is deliberately conservative: only **typed** signals
/// from `sea_orm::DbErr` and the modkit-db wrapper count. Unstructured
/// `RuntimeErr::Internal(String)` text — including driver messages like
/// `"connection closed by peer"` — stays in the `Internal` bucket;
/// string-matching driver text is fragile and the project's existing
/// classifiers (`is_retryable_contention`, `is_unique_violation`) are
/// SQLSTATE-typed for the same reason.
pub(crate) fn is_db_availability_error(err: &DbError) -> bool {
    // `DbError::Io(_)`: modkit-db's typed `std::io::Error` wrapper —
    // only emitted for genuine system-level IO failures (socket reset, etc.).
    // `DbErr::ConnectionAcquire(_)` covers `Timeout` and `ConnectionClosed`
    // (the only `ConnAcquireErr` variants).
    // `DbErr::Conn(_)` is sea-orm's documented "problem with the database
    // connection" discriminant — connection-level by definition.
    // `DbErr::Exec(_)` / `DbErr::Query(_)` wrap a `RuntimeErr` whose
    // layering hides whether the failure was connectivity or query-level,
    // so they fall through to the `Internal` bucket rather than guess.
    //
    // Note on `DbError::Sqlx(_)`: AM is `SeaORM`-only — connectivity
    // failures round-trip through `DbErr::ConnectionAcquire` /
    // `DbErr::Conn` (handled above) before they would surface as raw
    // `sqlx::Error`. Deconstructing the wrapped error here would
    // require depending on `sqlx` directly, which the project-wide
    // dylint `de0706_no_direct_sqlx` rule forbids — outside of
    // `modkit-db`, code must talk to the SecORM abstraction, not raw
    // `sqlx`. The variant therefore falls through to `Internal`.
    matches!(
        err,
        DbError::Io(_) | DbError::Sea(DbErr::ConnectionAcquire(_) | DbErr::Conn(_))
    )
}

/// Returns a non-secret string description of `err` suitable for the
/// `am.db` `warn!` log and for the `Internal::diagnostic` audit field.
///
/// Config-bearing variants (`UnknownDsn`, `InvalidConfig`,
/// `ConfigConflict`, `InvalidSqlitePragma`, `UnknownSqlitePragma`,
/// `InvalidParameter`, `SqlitePragma`, `EnvVar`, `UrlParse`) can carry
/// DSN strings, env-var names/values, or other operator-supplied text
/// that may include passwords / hostnames / tokens — their bodies are
/// dropped, only the variant kind survives. Pass-through wrappers
/// (`Sqlx`, `Sea`, `Io`, `Lock`, `Other`) are also reduced to a kind
/// label because their `Display` impls forward arbitrary driver text.
/// Variants whose `Display` payload is statically defined and known-safe
/// (`FeatureDisabled`, `ConnRequestedInsideTx`) round-trip verbatim.
///
/// Operators correlate by trace-id between the `am.db` log and the
/// Problem envelope; they read the *kind* from the redacted diagnostic
/// and the surrounding request context for the *what*.
pub(crate) fn redacted_db_diagnostic(err: &DbError) -> &'static str {
    match err {
        DbError::UnknownDsn(_) => "db error: unknown DSN (text redacted)",
        DbError::FeatureDisabled(_) => "db error: feature not enabled",
        DbError::InvalidConfig(_) => "db error: invalid configuration (text redacted)",
        DbError::ConfigConflict(_) => "db error: configuration conflict (text redacted)",
        DbError::InvalidSqlitePragma { .. } => {
            "db error: invalid SQLite pragma parameter (text redacted)"
        }
        DbError::UnknownSqlitePragma(_) => "db error: unknown SQLite pragma (text redacted)",
        DbError::InvalidParameter(_) => "db error: invalid connection parameter (text redacted)",
        DbError::SqlitePragma(_) => "db error: SQLite pragma error (text redacted)",
        DbError::EnvVar { .. } => "db error: environment variable error (text redacted)",
        DbError::UrlParse(_) => "db error: URL parse error (text redacted)",
        DbError::Sqlx(_) => "db error: sqlx (text redacted)",
        DbError::Sea(_) => "db error: sea-orm (text redacted)",
        DbError::Io(_) => "db error: io (text redacted)",
        DbError::Lock(_) => "db error: lock (text redacted)",
        DbError::Other(_) => "db error: other (text redacted)",
        DbError::ConnRequestedInsideTx => {
            "db error: connection requested inside active transaction"
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_conv_tests.rs"]
mod error_conv_tests;
