//! Fixtures shared by more than one of the storage layer's unit-test modules.
//!
//! **Shared, not central.** A helper lands here when a second test module needs
//! it, and stays beside its own tests otherwise: `query::write_tests` keeps the
//! guarded-statement readers, `query::list_tests` the page fixtures,
//! `query::aggregate_tests` the dimension witnesses. What is here is what the
//! write path, the read paths and the reconciliation path all build their rows
//! and scopes from — one spelling of a unit record, one lazy store, one scope
//! parser — so a fixture drifting cannot make two modules disagree about what
//! they are testing.
//!
//! Every item is `pub(crate)` under `cfg(test)` only; nothing here is compiled
//! into the plugin.

// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use usage_collector_sdk::AggregationFold;

use crate::infra::metrics::Metrics;
use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::migration_probe;
use crate::infra::storage::record_store::PgRecordStore;

/// A valid meter type id: the reserved base plus one derivation segment,
/// `~`-terminated, which is what `MeterTypeId::new` validates.
pub(crate) const VCPU_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";

/// The covered period every unit record below carries: one hour starting at
/// `2023-11-14T22:13:20Z` (unix `1_700_000_000`).
pub(crate) const WINDOW_START_UNIX: i64 = 1_700_000_000;
pub(crate) const WINDOW_END_UNIX: i64 = 1_700_003_600;

/// `[2023-11-14T00:00:00Z, 2023-11-15T00:00:00Z)` — the day that contains
/// every fixture's covered period.
pub(crate) const RANGE_FROM_UNIX: i64 = 1_699_920_000;
pub(crate) const RANGE_TO_UNIX: i64 = 1_700_006_400;

/// The tenant the scopes below admit.
pub(crate) const SCOPE_TENANT_A: &str = "11111111-1111-1111-1111-111111111111";
/// A second admitted tenant, so a scope can take the disjunctive shape
/// `authz::scope_to_odata_filter` actually compiles.
pub(crate) const SCOPE_TENANT_B: &str = "22222222-2222-2222-2222-222222222222";

/// A test quantity. Panics on an invalid literal, which is a test bug.
pub(crate) fn qty(text: &str) -> usage_collector_sdk::UsageQuantity {
    usage_collector_sdk::UsageQuantity::parse(text).expect("test quantity literal")
}

/// A store over a lazy pool: no connection is opened, so the pre-DB validation
/// paths under test return before any query is issued. The tiny acquire timeout
/// keeps an accidental DB touch from hanging the test.
pub(crate) fn lazy_store() -> PgRecordStore {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://user:pass@localhost/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    PgRecordStore::new(
        pool.clone(),
        Arc::new(Metrics::new(pool)),
        CancellationToken::new(),
        // The published default. Nothing on these paths reaches the guard --
        // they return before any statement is issued -- so this only has to be
        // a value `validate` accepts.
        120,
    )
}

/// The meter every pure (no-DB) fixture below is written under. The reference
/// is arbitrary and derived from nothing: the ADR bars deriving one from the
/// identifier, and the write path stores the pair it is handed.
pub(crate) fn unit_meter() -> usage_collector_sdk::MeterRef {
    usage_collector_sdk::MeterRef::new(
        uuid::Uuid::from_u128(0x5ee1_0000_0000_0002),
        usage_collector_sdk::MeterTypeId::new(VCPU_METER).expect("valid meter id"),
    )
}

/// Minimal in-memory `StoredUsageRecord` for pure (no-DB) unit tests.
///
/// Built as a `UsageRecord` and converted rather than assembled field by
/// field, so the entry identity still goes on through the projection.
pub(crate) fn unit_record(
    tenant: uuid::Uuid,
    idem: &str,
    seq: u128,
) -> usage_collector_sdk::StoredUsageRecord {
    usage_collector_sdk::UsageRecord {
        id: uuid::Uuid::from_u128(seq),
        gts_type_id: usage_collector_sdk::MeterTypeId::new(VCPU_METER).expect("valid meter id"),
        tenant_id: tenant,
        resource_ref: usage_collector_sdk::ResourceRef::new("res-1", "compute.vm")
            .expect("valid resource_ref"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: usage_collector_sdk::IdempotencyKey::new(idem).expect("valid idem key"),
        accepted_at: time::OffsetDateTime::from_unix_timestamp(WINDOW_END_UNIX).expect("valid ts"),
        origin: usage_collector_sdk::RecordOrigin::Live,
        invalidation: None,
        window_start: time::OffsetDateTime::from_unix_timestamp(WINDOW_START_UNIX)
            .expect("valid ts"),
        window_end: time::OffsetDateTime::from_unix_timestamp(WINDOW_END_UNIX).expect("valid ts"),
    }
    .into_stored(unit_meter().uuid)
}

/// A `UsageRecordRow` whose canonical fields equal `record`, carrying the
/// given stored `metadata` jsonb verbatim. Used to exercise `resolve_dedup_hit`'s
/// absorb/conflict/decode-failure paths without a database, and to build the
/// read paths' stored rows.
pub(crate) fn row_matching(
    record: &usage_collector_sdk::StoredUsageRecord,
    metadata: serde_json::Value,
) -> UsageRecordRow {
    let (invalidates, reason_code) = match record.invalidation.as_ref() {
        Some(i) => (Some(i.target), Some(i.reason.as_str().to_owned())),
        None => (None, None),
    };
    UsageRecordRow {
        id: record.id,
        tenant_id: record.tenant_id,
        gts_type_uuid: record.gts_type_uuid,
        type_key: 1,
        quantity: record.quantity.as_decimal(),
        window_start: record.window_start,
        window_end: record.window_end,
        resource_id: record.resource_ref.resource_id().to_owned(),
        resource_type: record.resource_ref.resource_type().to_owned(),
        subject_id: None,
        subject_type: None,
        idempotency_key: record.idempotency_key.as_str().to_owned(),
        invalidates,
        reason_code,
        origin: record.origin.as_str().to_owned(),
        entry_type: record.entry_type().as_str().to_owned(),
        accepted_at: record.window_end,
        metadata,
    }
}

/// Compile a scope the way the gateway hands one over: an
/// `toolkit_odata::ast::Expr`. `authz::scope_to_odata_filter` builds the same
/// AST from PDP constraints; a filter string is just its readable spelling, and
/// keeps these tests naming the predicate rather than assembling `Box<Expr>`
/// towers.
pub(crate) fn parse_scope(raw: &str) -> ast::Expr {
    toolkit_odata::parse_filter_string(raw)
        .unwrap_or_else(|e| panic!("the test's own scope must parse: {e}"))
        .into_expr()
}

/// The registry reference every pure read-path builder test binds. Arbitrary
/// and derived from nothing: the builders bind the `uuid` they are handed.
pub(crate) fn list_meter() -> uuid::Uuid {
    uuid::Uuid::from_u128(0x5ee1_0000_0000_0002)
}

pub(crate) fn list_range() -> usage_collector_sdk::TimeRange {
    usage_collector_sdk::TimeRange::new(
        time::OffsetDateTime::from_unix_timestamp(RANGE_FROM_UNIX).expect("valid ts"),
        time::OffsetDateTime::from_unix_timestamp(RANGE_TO_UNIX).expect("valid ts"),
    )
    .expect("a strictly ordered range")
}

/// A stand-in for the gateway's read fingerprint. Opaque to the plugin: it
/// covers the caller's `$filter` together with all three typed parameters, and
/// nothing here may interpret or recompute it.
///
/// **Deliberately not the shape the gateway mints**: a fixture in the real shape
/// would let an implementation start depending on that shape with no test
/// noticing, and the SPI says the value is opaque and "its shape is the gateway's
/// to change".
pub(crate) const READ_FINGERPRINT: &str = "sha256:0f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// A tenant distinct from [`SCOPE_TENANT_A`], so a reconciliation test can
/// tell the call's own `tenant_id` argument apart from a tenant the compiled
/// scope names.
pub(crate) const RECONCILIATION_TENANT: uuid::Uuid = uuid::Uuid::from_u128(0x99);

/// The gateway's default page order: `(window_end, id)`, both ascending.
pub(crate) fn canonical_order() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "window_end".to_owned(),
            dir: SortDir::Asc,
        },
        OrderKey {
            field: "id".to_owned(),
            dir: SortDir::Asc,
        },
    ])
}

/// A first-page query in the shape the gateway guarantees: a non-empty
/// single-direction order naming both canonical fields, and a fingerprint.
pub(crate) fn list_query() -> ODataQuery {
    ODataQuery::new()
        .with_order(canonical_order())
        .with_filter_hash(READ_FINGERPRINT.to_owned())
}

/// The fold's own query slot: no order, no page size, no fingerprint. The fold
/// paginates nothing, so [`list_query`]'s canonical order would be noise here.
pub(crate) fn fold_query() -> ODataQuery {
    ODataQuery::new()
}

/// Every fold the SDK declares, in variant order.
pub(crate) fn every_fold() -> Vec<AggregationFold> {
    let folds = vec![
        AggregationFold::Sum,
        AggregationFold::Count,
        AggregationFold::Min,
        AggregationFold::Max,
        AggregationFold::Latest,
    ];
    // The exhaustiveness witness: a sixth fold would otherwise red
    // `fold_select_expr` in another file and leave the "every fold" tests
    // quietly covering five of six.
    for fold in &folds {
        match fold {
            AggregationFold::Sum
            | AggregationFold::Count
            | AggregationFold::Min
            | AggregationFold::Max
            | AggregationFold::Latest => {}
        }
    }
    folds
}

/// The comma-separated entries of a SQL column list, verbatim.
pub(crate) fn names(list: &str) -> Vec<&str> {
    list.split(',').map(str::trim).collect()
}

/// [`names`], reduced to the name each entry decodes as: the alias of an
/// `expr AS alias` entry, and the entry itself otherwise.
///
/// `RECORD_COLUMNS`' cast entries carry the aliases `UsageRecordRow`'s fields are
/// looked up by, so a set comparison against `INSERT_COLUMNS` has to be over
/// these. Deliberately narrow: it recognizes ` AS ` and nothing else, so an entry
/// shaped some other way reds a comparison rather than being normalized into one.
pub(crate) fn decoded_names(list: &str) -> Vec<&str> {
    names(list)
        .into_iter()
        .map(|entry| entry.rsplit_once(" AS ").map_or(entry, |(_, alias)| alias))
        .collect()
}

/// Every inserted column paired with the type the write path must bind it as —
/// an array of it, which is how `UNNEST` takes it — **derived from
/// `migrations/0001_init.sql`** by [`migration_probe::insertable_columns`]
/// rather than from the code under test.
///
/// This is the whole point of the pairing test that reads it. Checking the
/// generated SQL against `INSERT_COLUMN_TYPES` proves only that the SQL was
/// built from that array — transpose two entries and both sides move together —
/// so a transposition has to be measured against something that does not move,
/// and a hand transcription of the DDL would be a second spelling of the schema.
///
/// One deliberate divergence, the only entry still written by hand: `metadata` is
/// `jsonb` in the table but travels as `text[]` and is cast `::jsonb` per row,
/// because `jsonb[]` array encoding is the thing being sidestepped.
pub(crate) fn ddl_column_types() -> Vec<(&'static str, &'static str)> {
    migration_probe::insertable_columns()
        .into_iter()
        .map(|(name, ty)| match name {
            // Asserted, not assumed: an unconditional substitution would keep
            // rewriting `metadata` to `text` if the DDL ever stopped saying
            // `jsonb`, and the "one deliberate divergence" above would be a
            // claim outliving the code.
            "metadata" => {
                assert_eq!(
                    ty, "jsonb",
                    "the metadata divergence is jsonb->text; the DDL now says {ty}"
                );
                (name, "text")
            }
            _ => (name, ty),
        })
        .collect()
}
