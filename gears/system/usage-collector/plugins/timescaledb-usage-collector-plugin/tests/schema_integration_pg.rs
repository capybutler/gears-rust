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

/// The dedup obligation is the ledger's own UNIQUE, over the 6-tuple verbatim.
///
/// The constraint definition is compared as text rather than by name alone: a
/// constraint keeping its name while losing a column is the failure this exists
/// to catch, and `usage_records_dedup_uniq` exists either way. `entry_type` is
/// the column that failure would most plausibly take: it is the one identity
/// input a withdrawal does not share with the entry it withdraws, so dropping
/// it makes every withdrawal collide with that entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dedup_unique_spans_the_six_tuple() {
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
        def,
        "UNIQUE (tenant_id, gts_type_id, idempotency_key, window_start, window_end, entry_type, \
         type_key)",
        "the dedup UNIQUE must span the 6-tuple dedup identity, in that order - the same \
         six inputs the entry id is a UUIDv5 projection of \
         (cpt-cf-usage-collector-adr-record-identity-derivation) - plus the partition key, \
         which a type determines and so separates no two rows the 6-tuple joins"
    );
}

/// The invalidation lookup index is partial and **not** unique: at most one
/// invalidation per entry is a dedup outcome of the shared identity - every
/// withdrawal of one entry repeats that entry's five other components under
/// `entry_type = invalidation` - not a store-side rule (the gear's DESIGN §3.1
/// "At most one invalidation").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_invalidation_lookup_index_is_partial_and_not_unique() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let def: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes \
         WHERE tablename = 'usage_records' AND indexname = 'usage_records_invalidates_idx'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_records_invalidates_idx must exist");
    assert!(
        !def.contains("UNIQUE"),
        "the lookup index must not be unique: {def}"
    );
    assert!(
        def.contains("btree (invalidates, window_end, type_key)"),
        "the index must lead with `invalidates`: {def}"
    );
    assert!(
        def.contains("WHERE (invalidates IS NOT NULL)"),
        "the index must be partial on `invalidates IS NOT NULL`: {def}"
    );

    let unique_over_invalidates: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE tablename = 'usage_records' \
         AND indexdef LIKE 'CREATE UNIQUE INDEX%' AND indexdef LIKE '%invalidates%'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("count unique indexes");
    assert_eq!(
        unique_over_invalidates, 0,
        "no unique index may cover `invalidates`"
    );
}

/// `entry_type` is a written `usage_entry_type` enum, and not a generated
/// column.
///
/// `DESIGN.md` §3.7: the column is "written from the dispatched entry's
/// declared kind and never derived from another column". The gear made the
/// entry type caller-supplied end to end; this column inferred it from
/// `invalidates` anyway, and so did an in-process key the Record Store built
/// from a stored row's columns. The column is written now, and that key is
/// gone: the write path keys on the entry `id`, which carries the kind because
/// the kind is one of the six inputs it is derived over.
///
/// It reads `attnotnull` in the same query, because a written column can be
/// NULL where the generated one could not, and a NULL kind defeats
/// `usage_records_invalidation_pairing` outright - see the comment on that
/// assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_type_is_a_written_enum_and_not_generated() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    // `pg_attribute` plus `format_type` rather than `information_schema`, per
    // this suite's convention and for the reason
    // `the_feed_order_column_is_an_xid8_the_database_stamps` gives: the
    // catalogs hand back `text`, where `information_schema.columns` would need
    // three `character_data` / `sql_identifier` domains decoded. It also reads
    // `attnotnull`, which `information_schema` splits into a fourth column and
    // which nothing else here would see.
    let (data_type, generated, not_null): (String, String, bool) = sqlx::query_as(
        "SELECT format_type(a.atttypid, a.atttypmod), a.attgenerated::text, a.attnotnull \
         FROM pg_attribute a \
         WHERE a.attrelid = 'usage_records'::regclass AND a.attname = 'entry_type' \
           AND a.attnum > 0 AND NOT a.attisdropped",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_records.entry_type must exist");
    assert_eq!(
        data_type, "usage_entry_type",
        "the kind is stored as the enum, not as a variable-length string"
    );
    assert_eq!(
        generated, "",
        "a generated column would derive the kind again (attgenerated is 's' for STORED)"
    );
    // `NOT NULL` is what makes `usage_records_invalidation_pairing` total. A
    // `CHECK` admits a row whose predicate evaluates to NULL, and with a NULL
    // `entry_type` both of that constraint's arms are NULL, so the row would be
    // stored declaring no kind at all and carrying whatever withdrawal fields
    // it liked - the disagreement the constraint exists to refuse. Under the
    // retired generated column the expression could not yield NULL, so this is
    // a property the written column introduced.
    //
    // Asserted here for the reason
    // `the_feed_order_column_is_an_xid8_the_database_stamps` gives about
    // `xact_id`: `ledger_columns()` reads the declared type and stops, so the
    // migration's own `NOT NULL` reaches no oracle, and the live table is where
    // it can be seen.
    assert!(
        not_null,
        "entry_type must be NOT NULL: a CHECK admits a row whose predicate is NULL, so a \
         NULL kind would walk straight through usage_records_invalidation_pairing"
    );

    let labels: Vec<String> = sqlx::query_scalar(
        "SELECT e.enumlabel FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid \
         WHERE t.typname = 'usage_entry_type' ORDER BY e.enumsortorder",
    )
    .fetch_all(&h.pool)
    .await
    .expect("enum labels");
    assert_eq!(labels, vec!["record".to_owned(), "invalidation".to_owned()]);
}

/// The pairing constraint refuses a declared kind that disagrees with the
/// withdrawal pair, in either direction.
///
/// `DESIGN.md` §3.7: `usage_records_invalidation_pairing` is
/// "`entry_type = 'invalidation'` exactly when `invalidates` and `reason_code`
/// are both set; an ordinary measurement carries neither", which "keeps the
/// declared kind and the withdrawal fields from disagreeing in storage".
///
/// Under the generated column this was unassertable: the kind was a function of
/// `invalidates`, so the two could not disagree by construction. Written, they
/// can, and this is what refuses it. Driven as raw SQL because the SPI cannot
/// express the disagreement - an entry's kind and its pair are two projections
/// of one `Option<Invalidation>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pairing_constraint_refuses_a_kind_that_disagrees_with_the_withdrawal_pair() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let err = common::insert_raw_entry(&h.pool)
        .entry_type("invalidation")
        .invalidates(None)
        .execute()
        .await
        .expect_err("a declared invalidation with no target must be refused");
    assert!(
        err.to_string()
            .contains("usage_records_invalidation_pairing"),
        "the pairing constraint must be what refuses a targetless invalidation, got: {err}"
    );

    // The other direction, which the `entry_type =` conjuncts are equally what
    // refuses: without them a row naming a target and a reason would be
    // admitted while declaring itself an ordinary measurement.
    let err = common::insert_raw_entry(&h.pool)
        .entry_type("record")
        .invalidates(Some(Uuid::from_u128(0x2100_0001)))
        .reason_code(Some("duplicate_submission"))
        .execute()
        .await
        .expect_err("a declared record that names a target must be refused");
    assert!(
        err.to_string()
            .contains("usage_records_invalidation_pairing"),
        "the pairing constraint must be what refuses a record naming a target, got: {err}"
    );
}

/// A row whose declared kind and withdrawal pair agree is admitted, in both
/// shapes.
///
/// The premise of the refusals above: they must fail on the disagreement and
/// not on something [`common::insert_raw_entry`] gets wrong for every row it
/// writes, which a refusal-only pair of assertions cannot tell apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pairing_constraint_admits_a_kind_that_agrees_with_the_withdrawal_pair() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    common::insert_raw_entry(&h.pool)
        .entry_type("record")
        .execute()
        .await
        .expect("an ordinary measurement carrying neither half must be admitted");

    common::insert_raw_entry(&h.pool)
        .entry_type("invalidation")
        .invalidates(Some(Uuid::from_u128(0x2100_0002)))
        .reason_code(Some("duplicate_submission"))
        .execute()
        .await
        .expect("a withdrawal carrying both halves must be admitted");
}

/// There is no usage-type catalog table, and this is a real assertion rather
/// than a formality.
///
/// Declarations live in `types-registry` and the storage SPI never sees one, so
/// nothing in this crate would fail to compile if the table came back. What it
/// catches is a **stale database surviving a migration change**: a container
/// reused across a schema edit, or a deployment migrated forward from the
/// superseded schema instead of rebuilt. Either leaves a table the plugin no
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

/// The retired per-scope acceptance counter left nothing behind: no
/// `usage_acceptance_sequence` table and no `usage_records_acceptance_seq_idx`.
///
/// An absence is as much a part of the §3.7 target schema as a declaration, and
/// it is the part with no other oracle. Nothing in this crate names either
/// object any more, so no compile and no other test in this file would notice a
/// migration edit that brought one back - the column-order test reads the
/// *table's* columns and sees neither a sibling table nor an index.
///
/// Neither would be inert if it returned. The counter table is a contended row
/// lock every write used to take, on a row shared by every writer of
/// one `(tenant_id, gts_type_id)` rather than by writers of one entry, which is
/// what made an unrelated busy meter able to serialise a tenant's ingest. The
/// index existed to order a fold on a column the fold no longer reads, and the
/// order it served is not the one the gear's `DESIGN.md` §3.1 states.
///
/// The column itself is covered by
/// [`the_live_columns_are_the_migrations_columns_in_order`], which reads the
/// live column list back against the migration's; these two are what the table
/// and the index need instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_retired_acceptance_counter_left_no_table_and_no_index() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let counter_table: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'usage_acceptance_sequence')",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_acceptance_sequence existence query");
    assert!(
        !counter_table,
        "usage_acceptance_sequence must not exist: feed order is the xid8 of the \
         inserting transaction and no path claims a per-scope number. Its presence \
         means a migration edit resurrected the counter, or a database survived a \
         schema change rather than being rebuilt."
    );

    // Named in `public`, which is where the migration would declare it. A
    // hypertable's chunk-local clones carry generated names in
    // `_timescaledb_internal` and are not what this asks about.
    let seq_index: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes \
         WHERE schemaname = 'public' AND indexname = 'usage_records_acceptance_seq_idx')",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_records_acceptance_seq_idx existence query");
    assert!(
        !seq_index,
        "usage_records_acceptance_seq_idx must not exist: the LATEST fold orders on \
         window_end, accepted_at and id (the gear's DESIGN section 3.1), none of \
         which this index leads with, and the column it was built over is gone."
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

    // The insertable subset is the same list minus the one column the ledger
    // writes itself, and this asserts the *reason* it is excluded rather than
    // the exclusion: `xact_id` is stamped by a column default. `accepted_at`
    // replaced the defaulted-and-omitted `ingested_at`; unlike it,
    // `accepted_at` is bound on every insert, so it stays in the insertable
    // set. `entry_type` is bound on every insert too, and is named here
    // because it used to be the other exclusion.
    let insertable: BTreeSet<&str> = migration_probe::insertable_columns()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(
        !insertable.contains("xact_id")
            && insertable.contains("accepted_at")
            && insertable.contains("entry_type"),
        "the insertable set must exclude the database-stamped column and keep \
         accepted_at and entry_type"
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
        !self_written.iter().any(|c| c == "entry_type"),
        "entry_type must carry neither a generation expression nor a DEFAULT in the \
         live table, because every insert writes it from the dispatched entry's \
         declared kind: {self_written:?}"
    );
    assert!(
        !self_written.iter().any(|c| c == "accepted_at"),
        "accepted_at must carry no DEFAULT in the live table — it is written from the \
         record on every insert: {self_written:?}"
    );
    assert!(
        self_written.iter().any(|c| c == "xact_id"),
        "xact_id must carry a DEFAULT in the live table, which is why \
         insertable_columns() drops it: {self_written:?}"
    );
}

/// The feed order's first key is an `xid8` the database stamps, not a value
/// the Record Store supplies.
///
/// `DESIGN.md` §3.1 makes both halves normative: it is "the `xid8` of the
/// transaction that inserted the entry, stamped by the database default
/// `pg_current_xact_id()` and never set by the Record Store". The type is
/// load-bearing as well as the default — `xid8` is 64-bit and totally
/// ordered, where `xid` wraps and compares only modulo 2^32, so a feed order
/// built on `xid` would reorder itself every wraparound. `PostgreSQL` agrees
/// far enough to refuse the index: substituting `xid` here fails the migration
/// with "data type xid has no default operator class for access method btree",
/// measured on `timescale/timescaledb:2.29.2-pg18`. That makes this assertion
/// the narrow guard against a type that is merely *wider* than the design
/// wants — `bigint` builds the index happily and loses the transaction
/// semantics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_feed_order_column_is_an_xid8_the_database_stamps() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    // `pg_attribute` plus `pg_attrdef` rather than `information_schema`, which
    // is this suite's convention throughout and which avoids decoding
    // `information_schema`'s `character_data` domains: `format_type` and
    // `pg_get_expr` both return `text`.
    let (data_type, column_default, not_null): (String, Option<String>, bool) = sqlx::query_as(
        "SELECT format_type(a.atttypid, a.atttypmod), pg_get_expr(d.adbin, d.adrelid), \
                a.attnotnull \
         FROM pg_attribute a \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         WHERE a.attrelid = 'usage_records'::regclass AND a.attname = 'xact_id' \
           AND a.attnum > 0 AND NOT a.attisdropped",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_records.xact_id must exist");

    assert_eq!(
        data_type, "xid8",
        "the feed order's first key must be xid8: xid wraps around and compares \
         only modulo 2^32, so it is not a total order"
    );
    assert_eq!(
        column_default.as_deref(),
        Some("pg_current_xact_id()"),
        "the default is what stamps it; without one the Record Store would have to, \
         and then no two entries of one batch would be guaranteed to share a value"
    );
    // The same property the marks table's `xact_id` carries, asserted here for
    // the same reason. `ledger_columns()` reads the declared type and stops, so
    // the migration's own `NOT NULL` reaches no oracle; the live table is where
    // it can be seen.
    assert!(
        not_null,
        "xact_id must be NOT NULL: a feed page selects `(xact_id, id) > $after` and orders \
         on it (DESIGN section 3.6), and a NULL compares NULL in both, so an entry with no \
         transaction id would be silently unservable rather than out of order"
    );
}

/// The ledger indexes `DESIGN.md` §3.7 declares outside a constraint, read back
/// as the database built them.
///
/// The population is named by its authority rather than counted, and both
/// halves of the qualifier do work. `usage_records_pkey` and
/// `usage_records_dedup_uniq` back constraints and reach `pg_indexes` that way,
/// so "every index on `usage_records`" is a larger set than this one. Of the
/// indexes that are left, `usage_records_invalidates_idx` is the one declared
/// with a predicate, and [`the_invalidation_lookup_index_is_partial_and_not_unique`]
/// reads its definition back - columns and `WHERE` clause both - so it is
/// covered there rather than duplicated into the loop below. A future
/// declaration joins this test by being added to `DESIGN.md`'s list, and no
/// number here has to be corrected for it.
///
/// `DESIGN.md` §3.7 declares every one of them over exactly these columns:
/// `usage_records_feed_idx (gts_type_id, xact_id, id)`, which "serves the feed
/// order"; `usage_records_watermark_idx (gts_type_id, tenant_id, accepted_at
/// DESC)`, which "serves the reconciliation acceptance watermark"; and
/// `usage_records_tenant_type_window_idx (tenant_id, gts_type_id, window_end
/// DESC)` with `usage_records_tenant_window_idx (tenant_id, window_end DESC)`,
/// which "support time-windowed reads". Column order is the whole point of each
/// — an index over the same columns in another order serves none of them — so
/// this reads every definition back rather than merely asserting the index
/// exists, and refuses a predicate on any of them: `indexdef` spells a partial
/// index as the same column list with a `WHERE` after it, so the column check
/// alone would pass one that covers only some of the rows it is relied on for.
///
/// **The two window indexes had no oracle at all until this assertion**, which
/// is why they are here rather than only in the migration. The first of them
/// carried a trailing tie-break column while the `LATEST` fold ordered on a
/// plugin-assigned sequence; the fold now orders on `accepted_at` and `id`,
/// neither of which is a leading key here, so the column went. Nothing would
/// have noticed it coming back: the column-order test reads the *table's*
/// columns, and a trailing index column is invisible to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ledger_indexes_declared_outside_a_constraint_are_built_in_their_declared_order() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    for (name, expected) in [
        ("usage_records_feed_idx", "(gts_type_id, xact_id, id)"),
        (
            "usage_records_watermark_idx",
            "(gts_type_id, tenant_id, accepted_at DESC)",
        ),
        (
            "usage_records_tenant_type_window_idx",
            "(tenant_id, gts_type_id, window_end DESC)",
        ),
        (
            "usage_records_tenant_window_idx",
            "(tenant_id, window_end DESC)",
        ),
    ] {
        let def: String =
            sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE indexname = $1")
                .bind(name)
                .fetch_one(&h.pool)
                .await
                .unwrap_or_else(|e| panic!("{name} must exist: {e}"));
        assert!(
            def.contains(expected),
            "{name} must be built over {expected}, got: {def}"
        );
        assert!(
            !def.contains(" WHERE "),
            "{name} is declared without a predicate, so it must be built without \
             one: a partial index over the same columns serves only the rows \
             matching its predicate and silently stops covering the rest. \
             got: {def}"
        );
    }
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

/// The feed retention marks table: one row per GTS type, keyed by the type,
/// holding a whole feed position.
///
/// `DESIGN.md` §3.7 keys it on `gts_type_id` and declares `xact_id` and `id`
/// both `NOT NULL`, because the pair is one position: a mark carrying half of
/// one names no entry, and the feed page compares positions rather than
/// transaction ids. The primary key is what makes "one row per GTS type that
/// has lost an entry to retention" a property of the table instead of a
/// convention the sweep is trusted to keep.
///
/// Created empty and still empty: the sweep raises a mark and `read_feed_page`
/// reads it, both in slice 3. Only the shape is asserted here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_feed_retention_marks_table_is_keyed_per_gts_type() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let columns: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT attname::text, format_type(atttypid, atttypmod), attnotnull \
         FROM pg_attribute \
         WHERE attrelid = 'usage_feed_retention_marks'::regclass \
           AND attnum > 0 AND NOT attisdropped \
         ORDER BY attnum",
    )
    .fetch_all(&h.pool)
    .await
    .expect("usage_feed_retention_marks must exist");

    assert_eq!(
        columns,
        vec![
            ("gts_type_id".to_owned(), "text".to_owned(), true),
            ("xact_id".to_owned(), "xid8".to_owned(), true),
            ("id".to_owned(), "uuid".to_owned(), true),
        ],
        "usage_feed_retention_marks is (gts_type_id text, xact_id xid8, id uuid), all NOT NULL"
    );

    let pk: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = 'usage_feed_retention_marks'::regclass AND contype = 'p'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_feed_retention_marks must have a primary key");
    assert_eq!(
        pk, "PRIMARY KEY (gts_type_id)",
        "one mark per GTS type: a key carrying the position too would let a type \
         hold two marks, and then no single row would be its highest deleted position"
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
    let store = common::record_store(&h);
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
        assert!(
            chunk.time_start < chunk.time_end && chunk.time_start <= common::fixture_window_end(),
            "the time range start bounds every window_end from below: {chunk:?}"
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

    // The assertion above is done with the policies; remove them so a
    // background refresh cannot race a later test's sweep.
    common::settle_and_remove_rollup_policies(&h.pool)
        .await
        .expect("settle and remove the re-created policies");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rollups_materialisation_table_resolves_to_a_hypertable() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let table = timescaledb_usage_collector_plugin::infra::storage::rollup_maintenance::materialization_table(&h.pool)
        .await
        .expect("lookup runs")
        .expect("the rollup exists");
    // `timescaledb_information.hypertables` excludes a continuous aggregate's
    // materialisation hypertable (its view definition filters
    // `ca.mat_hypertable_id IS NULL`, verified on 2.29.2), so the internal
    // catalog `retention_sweep.rs` already reads elsewhere is what actually
    // confirms this table is registered as a hypertable.
    let is_hypertable: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM _timescaledb_catalog.hypertable \
         WHERE format('%I.%I', schema_name, table_name) = $1)",
    )
    .bind(&table)
    .fetch_one(&h.pool)
    .await
    .expect("hypertable lookup");
    assert!(
        is_hypertable,
        "{table} must be the rollup's materialisation hypertable"
    );
}
