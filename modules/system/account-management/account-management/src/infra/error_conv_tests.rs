//! Tests for the infra-layer DB-error classification predicates.
//!
//! Exercises [`is_serialization_failure`] and [`is_db_availability_error`]
//! directly — these are the typed signals that
//! [`From<DomainError> for CanonicalError`] consumes at the boundary.
//! Boundary-mapping coverage (`DbErr` → `CanonicalError` category + HTTP
//! status) lives in `domain/error_tests.rs` and is intentionally kept
//! there alongside the mapping itself.

use super::{is_db_availability_error, is_serialization_failure};
use modkit_db::DbError;
use sea_orm::{ConnAcquireErr, DbErr};

#[test]
fn unclassified_db_err_is_not_serialization_failure() {
    let db_err = DbErr::Custom("nothing transient".into());
    assert!(!is_serialization_failure(&db_err));
}

#[test]
fn connection_acquire_timeout_is_db_availability_error() {
    let wrapped = DbError::Sea(DbErr::ConnectionAcquire(ConnAcquireErr::Timeout));
    assert!(is_db_availability_error(&wrapped));
}

#[test]
fn io_error_is_db_availability_error() {
    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by peer");
    let wrapped = DbError::Io(io_err);
    assert!(is_db_availability_error(&wrapped));
}

#[test]
fn custom_db_err_is_not_db_availability_error() {
    let wrapped = DbError::Sea(DbErr::Custom("query failed".into()));
    assert!(!is_db_availability_error(&wrapped));
}
