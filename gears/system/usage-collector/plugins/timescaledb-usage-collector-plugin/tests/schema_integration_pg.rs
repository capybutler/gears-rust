#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! What `migrations/0001_init.sql` actually built, read back out of a live
//! `TimescaleDB` after the migration ran. Requires Docker.
//!
//! Every assertion here is about the *database*, not about the SQL file: the
//! file is what `MIGRATOR` was handed, and these ask what `PostgreSQL` made of
//! it. The column-sequence check is the one that joins the two, against
//! [`migration_probe`] — the crate's single parse of that file — rather than a
//! second transcription of the column list into this suite.

mod common;

use std::collections::BTreeSet;

use rust_decimal::Decimal;
use uuid::Uuid;

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::migration_probe;
use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;
use timescaledb_usage_collector_plugin::infra::storage::retention_sweep;

/// The DDL spellings `format_type` renders differently, written out rather
/// than derived.
///
/// The value of comparing against [`migration_probe`] is that neither side is
/// computed from the other, so this table is the only thing this suite knows
/// about `PostgreSQL` type naming. It is deliberately not a general alias map:
/// `timestamptz` is the one spelling in this migration that differs, and a new
/// one arriving should red this test rather than be silently normalized away.
fn canonical_type(ddl_spelling: &str) -> &str {
    match ddl_spelling {
        "timestamptz" => "timestamp with time zone",
        "int" => "integer",
        other => other,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ledger_partitions_on_window_end_then_type_key() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    // Two dimensions, in order: `window_end` first, `type_key` second.
    // Asserting the *columns and their order* rather than only "is a
    // hypertable" is the whole point. `window_end` leads because the read
    // contract selects on the covered period's end alone
    // (`cpt-cf-usage-collector-adr-window-end-selection`), and partitioning on
    // `window_start` instead would leave every range read scanning chunks it
    // cannot need while every constraint carrying the partition column
    // silently changed meaning. `type_key` follows so a chunk can be dropped
    // by the retention of the types it holds.
    let dims: Vec<(String, i64)> = sqlx::query_as(
        "SELECT column_name, dimension_number FROM timescaledb_information.dimensions \
         WHERE hypertable_name = 'usage_records' ORDER BY dimension_number",
    )
    .fetch_all(&h.pool)
    .await
    .expect("hypertable dimensions query");

    assert_eq!(
        dims,
        vec![
            ("window_end".to_owned(), 1_i64),
            ("type_key".to_owned(), 2_i64),
        ],
        "usage_records must partition on window_end first and on the per-type key second: \
         the key is what lets a chunk be dropped by the retention of the types in it"
    );
}

/// Retention is per type and applied by the plugin's sweep, so no table-wide
/// `TimescaleDB` retention policy may be registered: one would drop every type
/// at a single horizon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_table_wide_retention_policy_is_registered() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("jobs query");
    assert_eq!(jobs, 0, "no table-wide retention policy may be registered");
}

/// The dedup obligation is the ledger's own UNIQUE, over the 5-tuple verbatim.
///
/// The constraint definition is compared as text rather than by name alone: a
/// constraint keeping its name while losing a column is the failure this exists
/// to catch, and `usage_records_dedup_uniq` exists either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dedup_unique_spans_the_five_tuple() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = 'usage_records'::regclass AND conname = 'usage_records_dedup_uniq'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_records_dedup_uniq must exist");

    assert_eq!(
        def, "UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end, type_key)",
        "the dedup UNIQUE must span the 5-tuple dedup identity, in that order - the same \
         five inputs the entry id is a UUIDv5 projection of \
         (cpt-cf-usage-collector-adr-record-identity-derivation) - plus the partition key, \
         which a type determines and so separates no two rows the 5-tuple joins"
    );
}

/// At most one accepted invalidation per entry, enforced by a partial unique
/// index rather than by a read-then-write in the store.
///
/// All three properties are asserted because each carries a different failure.
/// Lose `UNIQUE` and the index enforces nothing while still being a plausible
/// read index. Lose the partial predicate and the index carries an entry for
/// every ordinary measurement, for a clause only invalidations can satisfy.
/// Lose `window_end` and a hypertable refuses the index outright, which is why
/// the partition column is in it at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_at_most_one_invalidation_index_is_partial_and_unique() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let def: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes \
         WHERE tablename = 'usage_records' AND indexname = 'usage_records_one_invalidation_uniq'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_records_one_invalidation_uniq must exist");

    assert!(
        def.contains("CREATE UNIQUE INDEX"),
        "the at-most-one-invalidation index must be UNIQUE, or it enforces nothing: {def}"
    );
    assert!(
        def.contains("btree (invalidates, window_end, type_key)"),
        "the index must lead with `invalidates` and carry both partition columns: {def}"
    );
    assert!(
        def.contains("WHERE (invalidates IS NOT NULL)"),
        "the index must be partial on `invalidates IS NOT NULL`: {def}"
    );
}

/// `entry_type` is a **stored generated** column, which is what makes
/// `$filter=entry_type eq 'invalidation'` resolve to a real column.
///
/// `attgenerated = 's'` is the stored kind. A plain column of the same name
/// would serve the filter and could then disagree with `invalidates`, which is
/// the disagreement `cpt-cf-usage-collector-adr-append-only-invalidation`
/// forbids; a `VIRTUAL` one would not be indexable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_type_is_a_stored_generated_column_over_invalidates() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let generated: String = sqlx::query_scalar(
        "SELECT attgenerated::text FROM pg_attribute \
         WHERE attrelid = 'usage_records'::regclass AND attname = 'entry_type'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("entry_type must be a column of usage_records");
    assert_eq!(
        generated, "s",
        "entry_type must be GENERATED ALWAYS ... STORED (attgenerated = 's')"
    );

    let expr: String = sqlx::query_scalar(
        "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef d \
         JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
         WHERE d.adrelid = 'usage_records'::regclass AND a.attname = 'entry_type'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("the generated expression must be readable");
    assert!(
        expr.contains("invalidates IS NULL"),
        "entry_type must be derived from `invalidates` and from nothing else, so the \
         kind cannot disagree with the reference it is a projection of: {expr}"
    );
}

/// There is no usage-type catalog table, and this is a real assertion rather
/// than a formality.
///
/// Declarations live in `types-registry` and the storage SPI never sees one, so
/// nothing in this crate would fail to compile if the table came back. What it
/// catches is a **stale database surviving a migration change**: a container
/// reused across a schema edit, or a deployment migrated forward from the
/// pre-slice-4 schema instead of rebuilt. Either leaves a table the plugin no
/// longer writes and a foreign key it no longer expects, and the first symptom
/// would be an insert failing `23503` on a row that is perfectly well formed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn there_is_no_usage_type_catalog() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let catalog: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'usage_type_catalog')",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_type_catalog existence query");
    assert!(
        !catalog,
        "usage_type_catalog must not exist: declarations are resolved through \
         types-registry and this plugin owns no catalog. Its presence means a \
         database survived a migration change rather than being rebuilt."
    );

    // Its complement: nothing references anything. The retired catalog reached
    // the ledger through a foreign key, and a leftover FK is what would still
    // block chunk drops and reject well-formed rows.
    let fks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
         WHERE conrelid = 'usage_records'::regclass AND contype = 'f'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("foreign key count");
    assert_eq!(
        fks, 0,
        "usage_records must carry no foreign key; the catalog it used to reference is gone"
    );
}

/// The live table's column sequence is the migration's, read through the
/// crate's one parse of that file.
///
/// This is the only assertion here that joins the SQL text to the database:
/// everything above asks what `PostgreSQL` built, and [`migration_probe`] is
/// what the crate's `INSERT_COLUMNS` / `RECORD_COLUMNS` constants are checked
/// against. Without this, both halves can be internally consistent while the
/// migration that ran was a different file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_columns_are_the_migrations_columns_in_order() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let live: Vec<(String, String)> = sqlx::query_as(
        "SELECT attname::text, format_type(atttypid, atttypmod) FROM pg_attribute \
         WHERE attrelid = 'usage_records'::regclass AND attnum > 0 AND NOT attisdropped \
         ORDER BY attnum",
    )
    .fetch_all(&h.pool)
    .await
    .expect("live column query");

    let declared: Vec<(String, String)> = migration_probe::ledger_columns()
        .into_iter()
        .map(|(name, ty)| (name.to_owned(), canonical_type(ty).to_owned()))
        .collect();

    assert_eq!(
        live, declared,
        "the live ledger's columns must be the migration's, in declaration order"
    );

    // The insertable subset is the same list minus the two the ledger writes
    // itself, and this asserts the *reason* each is excluded rather than the
    // exclusion: `entry_type` is generated, `ingested_at` has a default.
    let insertable: BTreeSet<&str> = migration_probe::insertable_columns()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(
        !insertable.contains("entry_type") && !insertable.contains("ingested_at"),
        "the insertable set must exclude the generated column and the defaulted one"
    );
    let self_written: Vec<String> = sqlx::query_scalar(
        "SELECT attname::text FROM pg_attribute \
         WHERE attrelid = 'usage_records'::regclass AND attnum > 0 AND NOT attisdropped \
           AND (attgenerated <> '' OR (atthasdef AND attgenerated = '')) \
         ORDER BY attnum",
    )
    .fetch_all(&h.pool)
    .await
    .expect("self-written column query");
    // `metadata` also carries a DEFAULT and is nevertheless supplied per row,
    // so this is a superset check rather than an equality: what must hold is
    // that nothing the insert writes is a column the ledger computes.
    assert!(
        self_written.iter().any(|c| c == "entry_type")
            && self_written.iter().any(|c| c == "ingested_at"),
        "entry_type must be generated and ingested_at defaulted in the live table, which is \
         why insertable_columns() drops exactly those two: {self_written:?}"
    );
}

/// The per-type key table: one row per type, keyed by the type, with a
/// database-assigned integer that nothing else may write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_type_key_maps_each_type_to_a_generated_integer() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT attname::text, format_type(atttypid, atttypmod), attidentity::text \
         FROM pg_attribute \
         WHERE attrelid = 'usage_type_key'::regclass AND attnum > 0 AND NOT attisdropped \
         ORDER BY attnum",
    )
    .fetch_all(&h.pool)
    .await
    .expect("usage_type_key must exist");

    assert_eq!(
        columns,
        vec![
            ("gts_type_id".to_owned(), "text".to_owned(), String::new()),
            ("type_key".to_owned(), "integer".to_owned(), "a".to_owned()),
        ],
        "usage_type_key is (gts_type_id text, type_key int GENERATED ALWAYS AS IDENTITY)"
    );
}

/// The retention sweep reads chunk ranges out of `TimescaleDB`'s internal
/// catalog, whose shape changed between 2.17 and 2.29. This is the pin that
/// fails when an image upgrade reshapes it again: one row per chunk, each with
/// a time range end and a width-1 key range naming a mapped type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chunk_catalog_query_reads_one_row_per_chunk_with_both_ranges() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0xCA7A);
    store
        .create(common::entry(
            &common::meter(common::VCPU_METER),
            tenant,
            "vcpu",
            Decimal::ONE,
        ))
        .await
        .expect("create vcpu");
    store
        .create(common::entry(
            &common::meter(common::GB_METER),
            tenant,
            "gb",
            Decimal::ONE,
        ))
        .await
        .expect("create gb");

    let chunks = retention_sweep::list_chunks(&h.pool)
        .await
        .expect("the catalog query must run against this TimescaleDB image");
    let shown: i64 = sqlx::query_scalar("SELECT count(*) FROM show_chunks('usage_records')")
        .fetch_one(&h.pool)
        .await
        .expect("show_chunks");
    assert_eq!(shown, 2, "two types in one time range are two chunks");
    assert_eq!(
        i64::try_from(chunks.len()).unwrap(),
        shown,
        "one catalog row per chunk: {chunks:?}"
    );

    let keys: Vec<i32> = sqlx::query_scalar("SELECT type_key FROM usage_type_key")
        .fetch_all(&h.pool)
        .await
        .expect("mapped keys");
    for chunk in &chunks {
        assert_eq!(
            chunk.key_end - chunk.key_start,
            1,
            "slice width 1: {chunk:?}"
        );
        assert!(
            keys.iter().any(|k| i64::from(*k) == chunk.key_start),
            "each chunk's key range names a mapped type: {chunk:?} vs {keys:?}"
        );
        assert!(
            chunk.time_end > common::fixture_window_end(),
            "the time range end bounds every window_end in the chunk: {chunk:?}"
        );
    }
}

/// The rollup is a real-time continuous aggregate over the grain the read path
/// and the retention cut both rely on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rollup_is_a_real_time_continuous_aggregate_over_the_grain() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let materialized_only: bool = sqlx::query_scalar(
        "SELECT materialized_only FROM timescaledb_information.continuous_aggregates \
         WHERE view_schema = current_schema() AND view_name = 'usage_rollup_1h'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_rollup_1h must be a continuous aggregate");
    assert!(!materialized_only, "real-time aggregation must be on");

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT attname::text FROM pg_attribute \
         WHERE attrelid = 'usage_rollup_1h'::regclass AND attnum > 0 AND NOT attisdropped \
         ORDER BY attnum",
    )
    .fetch_all(&h.pool)
    .await
    .expect("view columns");
    assert_eq!(
        columns,
        [
            "bucket",
            "tenant_id",
            "gts_type_id",
            "type_key",
            "sum_value",
            "count_value"
        ]
    );
}

/// Setup registers exactly the two configured policies, and re-running it
/// replaces them rather than adding more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_registers_exactly_the_two_configured_refresh_policies() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let cfg = timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig {
        rollup_materialization_lag_secs: 10_800,
        rollup_live_window_secs: 172_800,
        rollup_refresh_interval_secs: 300,
        rollup_history_refresh_interval_secs: 7_200,
        ..h.cfg.clone()
    };
    apply_post_migration_setup(&h.pool, &cfg)
        .await
        .expect("setup once");
    apply_post_migration_setup(&h.pool, &cfg)
        .await
        .expect("setup twice");

    // (start offset secs or NULL, end offset secs, schedule secs), live first.
    let policies: Vec<(Option<f64>, f64, f64)> = sqlx::query_as(
        "SELECT EXTRACT(EPOCH FROM (config->>'start_offset')::interval)::float8, \
                EXTRACT(EPOCH FROM (config->>'end_offset')::interval)::float8, \
                EXTRACT(EPOCH FROM schedule_interval)::float8 \
         FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_refresh_continuous_aggregate' \
           AND hypertable_schema = current_schema() AND hypertable_name = 'usage_rollup_1h' \
         ORDER BY (config->>'start_offset') IS NULL, job_id",
    )
    .fetch_all(&h.pool)
    .await
    .expect("policy rows");
    assert_eq!(
        policies,
        vec![
            (Some(172_800.0), 10_800.0, 300.0),
            (None, 172_800.0, 7_200.0),
        ],
        "one live policy and one history policy, replaced on re-run"
    );
}
