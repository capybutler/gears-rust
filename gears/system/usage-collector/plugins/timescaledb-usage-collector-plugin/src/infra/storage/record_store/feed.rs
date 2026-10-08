//! Steps 2 to 4 of the feed page protocol, and the retention-mark check both
//! step 3 and step 6 reach.
//!
//! A child of [`super`] rather than a sibling, because both methods need
//! [`PgRecordStore`]'s metrics and error classification; being a descendant
//! module is what gives them that without widening a field. Steps 1, 5 and 6 —
//! the transaction's open, its `COMMIT`, and the authoritative autocommit
//! re-check after it — stay in [`RecordStore::feed_page`](super::PgRecordStore)
//! because they are what own the connection across the commit boundary.
//!
//! The SQL is not here: [`super::super::query::feed`] owns the page statement
//! and [`MARK_ABOVE_SQL`], and is unit-tested against their text with no
//! database.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (hypertable
// time-series, `time_bucket` aggregation, keyset pagination — see DESIGN.md). Tenant
// isolation is enforced by hand via parameterized `tenant_id` predicates and an
// allowlisted-identifier query builder (DESIGN.md §Injection-Safe Query Translation),
// not SecureConn/AccessScope.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::collections::HashSet;

use sqlx::AssertSqlSafe;
use sqlx::postgres::PgConnection;
use uuid::Uuid;

use usage_collector_sdk::{StoredUsageRecord, UsageCollectorPluginError};

use crate::domain::feed_position::MAX_UUID;
use crate::infra::storage::entity::FeedRecordRow;
use crate::infra::storage::mapper::record_row_to_model;
use crate::infra::storage::query::feed::MARK_ABOVE_SQL;
use crate::infra::storage::query::translate::{SqlBind, bind_one};

use super::PgRecordStore;

/// The subscription's types, deduplicated with the caller's order preserved.
///
/// `unnest($1::uuid[])` iterates per array *element*, so a repeated type would
/// deliver its rows twice. `sort_unstable` + `dedup` would also dedup but erase
/// the order `unnest` walks the array in.
pub(super) fn dedup_subscription_types(subscription: &[Uuid]) -> Vec<Uuid> {
    let mut seen = HashSet::with_capacity(subscription.len());
    let mut types = Vec::with_capacity(subscription.len());
    for m in subscription {
        if seen.insert(*m) {
            types.push(*m);
        }
    }
    types
}

/// One page read's outcome inside the open snapshot transaction: either a
/// mark already stood above the position (the fast path, step 3), or the page
/// statement ran and this carries its rows and disposition.
pub(super) struct InTransactionPage {
    /// The page's entries, already mapped to the SDK model. Empty when
    /// `marked` is `true`: the page statement is never sent on that path
    /// (`docs/DESIGN.md` §3.6's diagram wraps it in `opt not marked`).
    pub(super) entries: Vec<StoredUsageRecord>,
    /// The continuation, as a position pair. `None` only when `marked` is
    /// `true` (mirroring `entries`) or when a bounded replay reached its
    /// `until`.
    pub(super) next: Option<(u64, Uuid)>,
    /// Whether step 3's fast-path check already found a mark above the
    /// position. `RecordStore::feed_page` short-circuits step 6 on this: a mark
    /// the fast path found is certain, so no re-check confirms it.
    pub(super) marked: bool,
}

impl InTransactionPage {
    /// Step 3 found a mark above the position. No page statement is sent.
    fn marked() -> Self {
        Self {
            entries: Vec::new(),
            next: None,
            marked: true,
        }
    }
}

impl PgRecordStore {
    /// Steps 2 to 4 of the feed page protocol, inside the open snapshot
    /// transaction `conn` already holds (`RecordStore::feed_page`'s step 1).
    ///
    /// Returns early with [`InTransactionPage::marked`] when step 3 finds a
    /// mark, in which case the page statement is never sent: `docs/DESIGN.md`
    /// §3.6's diagram wraps the page statement in `opt not marked`, so an
    /// early mark refuses without building a page.
    ///
    /// Over clippy's argument limit: each parameter is a distinct input the
    /// six-step protocol names, and this is a private helper with one call site,
    /// so bundling them into a struct would be indirection for its own sake.
    #[allow(clippy::too_many_arguments)]
    // @cpt-algo:cpt-cf-uc-plugin-algo-next-position-selection:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-snapshot-and-bounded-replay:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-live-head-position:p1
    pub(super) async fn feed_page_in_transaction(
        &self,
        conn: &mut PgConnection,
        sql: &str,
        binds: &[SqlBind],
        types: &[Uuid],
        after: Option<(u64, Uuid)>,
        until: Option<(u64, Uuid)>,
        limit: u64,
    ) -> Result<InTransactionPage, UsageCollectorPluginError> {
        // Step 2. Fixes the snapshot and yields the settled horizon. Read as
        // text because `xid8` has no `sqlx` Decode, which is the same reason
        // `FeedRecordRow` holds `xact_id` as a String.
        let horizon: String =
            sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text")
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| self.record_backend_error(&e))?;
        let horizon_num: u64 = horizon.parse().map_err(|_| {
            UsageCollectorPluginError::internal(format!(
                "the settled horizon `{horizon}` read off pg_snapshot_xmin did not parse as a u64"
            ))
        })?;

        // Step 3. A fast path only: this transaction's own snapshot can miss
        // a drop that commits after it, which is why step 6 re-checks in
        // autocommit after COMMIT.
        if let Some(position) = after
            && self.mark_stands_above(&mut *conn, types, position).await?
        {
            return Ok(InTransactionPage::marked());
        }

        // Step 4. `persistent(false)` so this call never *inserts* a plan into
        // the connection's statement cache, and nothing else in this crate
        // prepares this SQL text persistently, so there is no stale entry to
        // find: the statement is planned fresh after step 2. TimescaleDB
        // excludes chunks at plan time against the catalog it then sees, so a
        // plan cached before a chunk existed could silently skip that chunk's
        // rows — the failure §3.6's "Why" paragraph rules out.
        let mut q = sqlx::query_as::<_, FeedRecordRow>(AssertSqlSafe(sql)).persistent(false);
        q = q.bind(types).bind(&horizon);
        if let Some((xact_id, id)) = after {
            q = q.bind(xact_id.to_string()).bind(id);
        }
        if let Some((xact_id, id)) = until {
            q = q.bind(xact_id.to_string()).bind(id);
        }
        for b in binds {
            q = bind_one(q, b);
        }
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let limit_as_usize = usize::try_from(limit).unwrap_or(usize::MAX);
        let row_count = rows.len();
        // Captured before `row.record` is moved into `record_row_to_model`
        // below: `xact_id` lives on `FeedRecordRow` alone, not on the
        // `UsageRecordRow` that function consumes nor on the SDK model.
        let last_position = rows
            .last()
            .map(|row| -> Result<(u64, Uuid), UsageCollectorPluginError> {
                let xact_id: u64 = row.xact_id.parse().map_err(|_| {
                    UsageCollectorPluginError::internal(format!(
                        "a fed row's xact_id `{}` did not parse as a u64",
                        row.xact_id
                    ))
                })?;
                Ok((xact_id, row.record.id))
            })
            .transpose()?;

        let mut entries = Vec::with_capacity(row_count);
        for row in rows {
            entries.push(record_row_to_model(row.record)?);
        }

        // The disposition, in DESIGN §3.6's own order: a bounded replay that
        // reached its `until` closes; a page filled to its limit continues
        // from its last entry's own position; a short page has reached the
        // horizon and continues from the head position.
        //
        // "Reached `until`" is not the same test as "short": a page can fill to
        // exactly `limit` rows with its last row's position equal to `until`.
        // Testing `row_count < limit_as_usize` alone would fall through to the
        // filled-to-limit arm and mint a continuation for a closed replay.
        let next = if until.is_some() && (row_count < limit_as_usize || last_position == until) {
            None
        } else if row_count >= limit_as_usize {
            Some(last_position.ok_or_else(|| {
                UsageCollectorPluginError::internal(
                    "a feed page filled to its limit carried no rows, which the caller's \
                     limit >= 1 obligation should make unreachable",
                )
            })?)
        } else {
            Some((horizon_num.saturating_sub(1), MAX_UUID))
        };

        Ok(InTransactionPage {
            entries,
            next,
            marked: false,
        })
    }

    /// Whether a mark of any subscribed type stands above `position`.
    ///
    /// One small statement ([`MARK_ABOVE_SQL`]), run twice per page at most:
    /// once under the page's own snapshot as a fast path
    /// ([`Self::feed_page_in_transaction`], step 3) and once in autocommit as
    /// the authority (`RecordStore::feed_page`, step 6), both against the same
    /// pooled connection before and after its `COMMIT`.
    // @cpt-flow:cpt-cf-uc-plugin-flow-resume-after-retention:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-retention-mark-check:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-retention-refusal:p1
    pub(super) async fn mark_stands_above(
        &self,
        conn: &mut PgConnection,
        types: &[Uuid],
        position: (u64, Uuid),
    ) -> Result<bool, UsageCollectorPluginError> {
        let found: Option<i32> = sqlx::query_scalar(MARK_ABOVE_SQL)
            .bind(types)
            .bind(position.0.to_string())
            .bind(position.1)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;
        Ok(found.is_some())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "feed_tests.rs"]
mod feed_tests;
