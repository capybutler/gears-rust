//! Unit tests for the ledger's column sequences. Pure (no DB).
//!
//! The hazard these keep out is a name transposed between the column list, the
//! `SELECT` list and the `UNNEST` alias: two `text[]` columns exchanged that way
//! bind the wrong values, Postgres accepts it, and every row-level test still
//! passes. Hoisting them onto one [`INSERT_COLUMNS`] makes the transposition
//! unrepresentable; these assertions keep it that way.
//!
//! What is **not** here: the statements these compose into. The guarded
//! insert's own shape — that it inserts only what its guard admitted, that it
//! binds no `xact_id`, that its three column sequences agree — is
//! `super::super::query::write_tests`, next to the builder that emits it. The
//! `.bind()` sequence needs a live backend:
//! `records_ingest_integration_pg::a_row_written_through_the_batch_insert_reads_back_column_for_column`.

use std::collections::BTreeSet;

use super::{FEED_COLUMNS, INSERT_COLUMN_TYPES, INSERT_COLUMNS, RECORD_COLUMNS};
use crate::infra::storage::test_support::{ddl_column_types, decoded_names, names};

#[test]
fn each_inserted_column_is_bound_as_the_type_the_migration_declares() {
    let ddl = ddl_column_types();
    let want_names: Vec<&str> = ddl.iter().map(|(n, _)| *n).collect();
    let want_types: Vec<&str> = ddl.iter().map(|(_, t)| *t).collect();

    assert_eq!(
        names(INSERT_COLUMNS),
        want_names,
        "the inserted column sequence must be the migration's, in its order"
    );
    assert_eq!(
        INSERT_COLUMN_TYPES.to_vec(),
        want_types,
        "each column's bound type must be the one the migration declares for it \
         (metadata excepted: jsonb in the table, carried as text and cast per row)"
    );
}

#[test]
fn the_plain_read_list_names_exactly_the_written_columns() {
    // `RECORD_COLUMNS` is the *same* column set as `INSERT_COLUMNS`, cast where
    // decoding needs it, not a superset: `xact_id` lives only on `FEED_COLUMNS`.
    //
    // The comparison is over the name each entry *decodes as*, because one read
    // list entry is a select expression: `entry_type::text AS entry_type`. The
    // cast is load-bearing — `usage_entry_type` is a Postgres enum, which `sqlx`
    // refuses to decode into a `String`. The alias is not, since `PostgreSQL`
    // names a bare column cast's output after the column anyway, but it is pinned
    // because that is a naming rule of the server while `UsageRecordRow`'s fields
    // are looked up by name.
    let insert_names: BTreeSet<&str> = names(INSERT_COLUMNS).into_iter().collect();
    let record_names: BTreeSet<&str> = decoded_names(RECORD_COLUMNS).into_iter().collect();
    assert_eq!(
        record_names, insert_names,
        "RECORD_COLUMNS must name exactly the columns INSERT_COLUMNS writes, \
         no more and no fewer: left is what RECORD_COLUMNS decodes as, right \
         is what every insert writes"
    );
    // The comparison above is over sets, which de-duplicate before they compare:
    // a repeated entry would pass it, so the multiplicity is asserted directly.
    assert_eq!(
        decoded_names(RECORD_COLUMNS).len(),
        record_names.len(),
        "RECORD_COLUMNS must name each column exactly once"
    );
    assert_eq!(
        names(INSERT_COLUMNS).len(),
        INSERT_COLUMN_TYPES.len(),
        "one type per inserted column, in the same order"
    );
    // The cast, pinned as the literal select expression it is --
    // `decoded_names` above deliberately cannot see it.
    assert!(
        names(RECORD_COLUMNS).contains(&"entry_type::text AS entry_type"),
        "`entry_type::text AS entry_type` must be read back exactly so, cast \
         and alias: {RECORD_COLUMNS}"
    );
    assert_eq!(
        names(INSERT_COLUMNS).get(2),
        Some(&"gts_type_uuid"),
        "the inserted column list is the migration's declaration order, and \
         `0001_init.sql` declares the meter's reference third. Nothing pins \
         what is *last* on purpose: `metadata` is, and the `input` CTE's cast \
         deliberately no longer leans on that position - see \
         `query::write_tests::both_input_ctes_carry_the_column_list_with_metadata_the_one_cast_entry`"
    );
}

#[test]
fn the_feed_read_list_is_the_plain_one_plus_the_feed_orders_own_key() {
    // `xact_id` is the whole of the difference between `FEED_COLUMNS` and
    // `RECORD_COLUMNS`.
    let record_names: BTreeSet<&str> = decoded_names(RECORD_COLUMNS).into_iter().collect();
    let feed_names: BTreeSet<&str> = decoded_names(FEED_COLUMNS).into_iter().collect();
    assert!(
        record_names.is_subset(&feed_names),
        "every plain-read column must also be in the feed read list: missing {:?}",
        record_names.difference(&feed_names).collect::<Vec<_>>()
    );
    assert_eq!(
        feed_names.difference(&record_names).collect::<Vec<_>>(),
        vec![&"xact_id_text"],
        "the feed read list may add only the one column the feed order needs, \
         decoded under its own alias"
    );
    assert_eq!(
        decoded_names(FEED_COLUMNS).len(),
        feed_names.len(),
        "FEED_COLUMNS must name each column exactly once"
    );

    // Pinned as the literal select expression it is: `decoded_names` cannot see
    // it, and reusing `xact_id` as this cast's own alias would make
    // `query/feed.rs`'s `ORDER BY xact_id, id` resolve to this *output* column and
    // sort lexicographically over digit strings rather than numerically over the
    // `xid8` one — `PostgreSQL` binds a bare `ORDER BY` name matching both an
    // output and an input column to the output.
    assert!(
        names(FEED_COLUMNS).contains(&"xact_id::text AS xact_id_text"),
        "`xact_id::text AS xact_id_text` must be read back exactly so, cast \
         and aliased to a name distinct from its own source column: {FEED_COLUMNS}"
    );
}
