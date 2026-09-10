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

use timescaledb_usage_collector_plugin::infra::storage::migration_probe;

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
        other => other,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ledger_is_a_hypertable_partitioned_on_window_end() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    // One dimension, and it is `window_end`. Asserting the *column* rather than
    // only "is a hypertable" is the whole point: the read contract selects on
    // the covered period's end alone
    // (`cpt-cf-usage-collector-adr-window-end-selection`), and partitioning on
    // `window_start` instead would leave every range read scanning chunks it
    // cannot need while every constraint carrying the partition column silently
    // changed meaning.
    let dims: Vec<(String, i64)> = sqlx::query_as(
        "SELECT column_name, dimension_number FROM timescaledb_information.dimensions \
         WHERE hypertable_name = 'usage_records' ORDER BY dimension_number",
    )
    .fetch_all(&h.pool)
    .await
    .expect("hypertable dimensions query");

    assert_eq!(
        dims,
        vec![("window_end".to_owned(), 1_i64)],
        "usage_records must be a hypertable with exactly one time dimension, on window_end"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retention_policy_is_registered_against_the_ledger() {
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
    assert_eq!(
        jobs, 1,
        "exactly one retention policy must be registered against usage_records"
    );
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
        def, "UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end)",
        "the dedup UNIQUE must span the 5-tuple dedup identity, in that order - the same \
         five inputs the entry id is a UUIDv5 projection of \
         (cpt-cf-usage-collector-adr-record-identity-derivation)"
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
        def.contains("btree (invalidates, window_end)"),
        "the index must lead with `invalidates` and carry the partition column: {def}"
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
