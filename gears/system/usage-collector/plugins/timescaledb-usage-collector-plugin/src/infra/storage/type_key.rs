//! The plugin-internal integer key of each GTS type (`usage_type_key`).
//!
//! The key is the ledger hypertable's second partitioning dimension. It names a
//! type and never changes, so a resolved key is kept for the life of the
//! process and no entry ever expires. Only the write path uses this cache: the
//! read paths resolve the key inside their own statement
//! ([`super::query::push_meter_and_range_clauses`]).

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (see
// `record_store.rs`).
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};

use sqlx::PgConnection;

/// Assigns a key to a type that has none. A concurrent assignment of the same
/// type waits on the speculative row and then does nothing, so the read that
/// follows sees exactly one key.
pub const ASSIGN_TYPE_KEY_SQL: &str =
    "INSERT INTO usage_type_key (gts_type_id) VALUES ($1) ON CONFLICT (gts_type_id) DO NOTHING";

/// Reads a type's key.
pub const READ_TYPE_KEY_SQL: &str = "SELECT type_key FROM usage_type_key WHERE gts_type_id = $1";

/// Process-wide cache of resolved type keys.
#[derive(Debug, Default)]
pub struct TypeKeyCache {
    keys: RwLock<HashMap<String, i32>>,
}

impl TypeKeyCache {
    /// The key already resolved for `gts_type_id`, if any.
    #[must_use]
    pub fn cached(&self, gts_type_id: &str) -> Option<i32> {
        self.keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(gts_type_id)
            .copied()
    }

    /// The key of `gts_type_id`, assigning one on its first write.
    ///
    /// Runs in autocommit on `conn`, so call it **before** opening the write
    /// transaction: a key assigned inside a transaction that then rolls back
    /// would make every concurrent first writer of the type wait on it for
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns the `sqlx` error of either statement.
    pub async fn resolve(
        &self,
        conn: &mut PgConnection,
        gts_type_id: &str,
    ) -> Result<i32, sqlx::Error> {
        if let Some(key) = self.cached(gts_type_id) {
            return Ok(key);
        }
        sqlx::query(ASSIGN_TYPE_KEY_SQL)
            .bind(gts_type_id)
            .execute(&mut *conn)
            .await?;
        let key: i32 = sqlx::query_scalar(READ_TYPE_KEY_SQL)
            .bind(gts_type_id)
            .fetch_one(&mut *conn)
            .await?;
        self.remember(gts_type_id, key);
        Ok(key)
    }

    fn remember(&self, gts_type_id: &str, key: i32) {
        self.keys
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(gts_type_id.to_owned(), key);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "type_key_tests.rs"]
mod type_key_tests;
