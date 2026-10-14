//! Unit tests for the guarded insert statement and the conflict read-back.
//! Pure (no DB): every assertion is over the built SQL text.
//!
//! These pin the shape `DESIGN.md` §3.6 obliges — that the verdict is computed
//! inside the statement from its own clock, that only admitted rows reach the
//! arbiter, that the outer select carries each input row's verdict, and that
//! the three places the statement names its columns name one sequence. The
//! column sequences themselves have their own oracle in
//! [`super::super::columns`]' tests; the `.bind()` sequence needs a live
//! backend:
//! `records_ingest_integration_pg::a_row_written_through_the_batch_insert_reads_back_column_for_column`.

use super::{BATCH_CONFLICT_READ_SQL, BATCH_GUARDED_SQL};
use crate::infra::storage::columns::{INSERT_COLUMN_TYPES, INSERT_COLUMNS, RECORD_COLUMNS};
use crate::infra::storage::test_support::{ddl_column_types, decoded_names, names};

/// The guarded statements the write path builds.
///
/// There is one, and the helper is kept as a sequence rather than collapsed into
/// the constant so the shape assertions below stay written over every statement
/// DESIGN section 3.6 obliges. The label is what a failure names.
fn guarded_statements() -> [(&'static str, &'static str); 1] {
    [("batch", BATCH_GUARDED_SQL.as_str())]
}

/// The substring between `open` and the next `close`, or a panic naming what
/// was looked for. A reading convenience over a `split_once` chain, which every
/// statement-shape assertion below would otherwise spell out.
fn between<'a>(sql: &'a str, open: &str, close: &str) -> &'a str {
    sql.split_once(open)
        .and_then(|(_, rest)| rest.split_once(close))
        .unwrap_or_else(|| unreachable!("no `{open}` … `{close}` in: {sql}"))
        .0
}

#[test]
fn both_guarded_statements_insert_only_the_rows_their_own_guard_admitted() {
    // The obligations DESIGN section 3.6 puts on the write statement, each a
    // separate assertion because each fails on its own: a statement can compute
    // the verdict and insert regardless of it, or gate the insert on a verdict a
    // `SELECT statement_timestamp()` computed a statement earlier.
    //
    // The guard expression is transcribed by hand rather than read off
    // `admitted_expr`, which would move with any mutation of it. Only the
    // placeholder index is derived, off the migration's column list, because that
    // one really does move when the ledger gains a written column.
    let slack = ddl_column_types().len() + 1;
    let want_guard = format!(
        "(abs(extract(epoch FROM (t.accepted_at - statement_timestamp()))) <= ${slack}::bigint) \
         AS admitted"
    );
    for (label, sql) in guarded_statements() {
        assert!(
            sql.contains(&want_guard),
            "{label}: the verdict is computed inside the statement, from the \
             statement's own clock, and in either direction: {sql}"
        );
        assert!(
            sql.contains("SELECT id, tenant_id"),
            "{label}: the insert selects from the input CTE: {sql}"
        );
        assert!(
            sql.contains(" FROM input WHERE admitted ON CONFLICT ("),
            "{label}: only admitted rows are offered to the arbiter, so a \
             refused row never reaches the ledger: {sql}"
        );
        assert!(
            sql.contains(
                "SELECT input.id AS input_id, input.admitted, (ins.id IS NOT NULL) AS won,"
            ),
            "{label}: the outer select carries each input row's verdict and \
             whether it won: {sql}"
        );
        assert!(
            sql.ends_with(" FROM input LEFT JOIN ins USING (id)"),
            "{label}: inserted rows are aligned to input in SQL, on the derived \
             identity: {sql}"
        );
        assert_eq!(
            sql.matches("statement_timestamp()").count(),
            1,
            "{label}: one clock read, and it is the guard's: {sql}"
        );
        // The highest placeholder the statement carries, which is the slack's.
        // An independent oracle for the obligation above: a guard reading its
        // instant from a bind rather than from the statement would need one
        // placeholder more, and would say so here without touching the literal.
        let highest = sql
            .match_indices('$')
            .filter_map(|(i, _)| {
                sql[i + 1..]
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .filter(|d| !d.is_empty())
                    .and_then(|d| d.parse::<usize>().ok())
            })
            .max()
            .unwrap_or_else(|| unreachable!("{label}: the statement binds nothing: {sql}"));
        assert_eq!(
            highest, slack,
            "{label}: the last bind is the acceptance slack, and nothing after \
             it: {sql}"
        );
        assert!(
            !names(INSERT_COLUMNS).contains(&"xact_id"),
            "{label}: the store writes no xact_id; the column default stamps it"
        );
    }
}

#[test]
fn neither_guarded_statement_binds_the_column_the_database_stamps() {
    // `xact_id` is read back and never written. Asserted over the statement text
    // rather than over `INSERT_COLUMNS` alone, because a *write* could name it in
    // the insert's column list, the input CTE, or the SELECT that feeds the insert
    // from it. Nothing here counts occurrences — the read side names it freely.
    for (label, sql) in guarded_statements() {
        let insert_cols = between(sql, "usage_records (", ")");
        assert!(
            !names(insert_cols).contains(&"xact_id"),
            "{label}: the insert must not name xact_id: {insert_cols}"
        );
        let input_select = between(sql, "WITH input AS (SELECT ", ", (abs(");
        assert!(
            !input_select.contains("xact_id"),
            "{label}: the input CTE must not carry an xact_id: {input_select}"
        );
        let insert_select = between(sql, ") SELECT ", " FROM input WHERE admitted");
        assert!(
            !insert_select.contains("xact_id"),
            "{label}: the insert must not select an xact_id into the ledger: \
             {insert_select}"
        );
    }
}

#[test]
fn the_batch_write_names_one_column_sequence_in_all_three_places() {
    let sql = BATCH_GUARDED_SQL.as_str();
    let expected = names(INSERT_COLUMNS);

    assert_eq!(
        names(between(sql, "usage_records (", ")")),
        expected,
        "INSERT column list"
    );
    assert_eq!(
        names(between(sql, " AS t(", ")")),
        expected,
        "UNNEST alias list"
    );

    let unnest = between(sql, " FROM UNNEST(", ") AS t(");
    let params = names(unnest);
    assert_eq!(
        params.len(),
        expected.len(),
        "one UNNEST array per column: {unnest}"
    );
    for (i, (param, ty)) in params.iter().zip(INSERT_COLUMN_TYPES).enumerate() {
        assert_eq!(
            *param,
            format!("${}::{ty}[]", i + 1),
            "UNNEST parameter {} must be the array of column {}'s type",
            i + 1,
            i + 1
        );
    }

    // The arbiter, against the literal: the builder reads
    // `DEDUP_CONFLICT_TARGET`, so this cannot drift today, but an arbiter
    // hardcoded into the statement would otherwise pass.
    assert!(
        sql.contains(
            "ON CONFLICT (tenant_id, gts_type_uuid, idempotency_key, window_start, window_end, entry_type, type_key)"
        ),
        "the arbiter is the dedup 6-tuple: {sql}"
    );
}

#[test]
fn both_input_ctes_carry_the_column_list_with_metadata_the_one_cast_entry() {
    // The `input` CTE's columns are what the inner `SELECT <INSERT_COLUMNS> FROM
    // input` reads, so their names are load-bearing. What makes a transposition
    // unrepresentable is that the select list is built *from* `INSERT_COLUMNS`,
    // entry by entry and in its order; the alias on the cast entry stops the name
    // being inherited from PostgreSQL's rule for a bare column cast.
    //
    // **A trailing `::jsonb AS metadata` on the whole list** binds to its final
    // identifier and works only while `metadata` is last, so a migration appending
    // a column after it would cast the wrong one. That is deliberately not relied
    // on: this asserts the name-carrying form.
    for (label, sql) in guarded_statements() {
        let select_list = between(sql, "WITH input AS (SELECT ", ", (abs(");
        assert_eq!(
            decoded_names(select_list),
            names(INSERT_COLUMNS),
            "{label}: the input CTE's columns are the inserted column list, in \
             its order, whatever each entry is spelled as"
        );
        assert!(
            names(select_list).contains(&"metadata::jsonb AS metadata"),
            "{label}: `metadata` is the one entry carrying a cast, and it \
             carries its own alias rather than inheriting one: {select_list}"
        );
        assert_eq!(
            names(select_list)
                .iter()
                .filter(|entry| entry.contains("::"))
                .count(),
            1,
            "{label}: exactly one entry of the input select list is cast: \
             {select_list}"
        );
    }
}

#[test]
fn both_outer_selects_read_every_record_column_off_the_inserted_row() {
    // A not-won row's record columns must all be null, which is only true while
    // every one of them is qualified `ins.`. Unqualified, `USING (id)` merges
    // `id` to the left side's -- never null -- and the other names resolve to
    // `input`, so a lost row would read back as the row that was never written.
    for (label, sql) in guarded_statements() {
        let outer = between(sql, ") AS won, ", " FROM input LEFT JOIN");
        let want: Vec<String> = decoded_names(RECORD_COLUMNS)
            .into_iter()
            .map(|n| format!("ins.{n}"))
            .collect();
        assert_eq!(
            names(outer),
            want,
            "{label}: the outer select reads the whole read list off `ins`"
        );
    }
}

#[test]
fn the_conflict_read_back_keys_on_the_id_and_prunes_on_the_partition_column() {
    // Two claims, and they fail separately.
    //
    // *The key.* DESIGN section 3.6 requires the read-back to select on `id`.
    // Asserted as a literal predicate rather than by looking for the substring
    // `id`, which every column list already contains.
    //
    // *The pruning.* `usage_records` is partitioned on `window_end`, so a
    // predicate that does not constrain it cannot exclude a chunk. This is the only
    // thing that would notice the column being "simplified" out: the read-back
    // still returns the right row, just after touching every chunk the deployment
    // retains, inside the write transaction.
    let batch = BATCH_CONFLICT_READ_SQL.as_str();
    assert!(
        batch.contains("WHERE window_end = ANY($2::timestamptz[])"),
        "the batch read-back carries the conjunct the planner excludes chunks \
         with: {batch}"
    );
    assert!(
        batch.contains(
            "AND (id, window_end) IN (SELECT t1, t2 FROM UNNEST($1::uuid[], \
             $2::timestamptz[]) AS t(t1, t2))"
        ),
        "and pairs each id with its own period, which a second `= ANY` could \
         not: {batch}"
    );

    // It may not read the ledger through anything but the read list, so a
    // column added to `RECORD_COLUMNS` reaches it without a second edit.
    assert!(
        batch.starts_with(&format!("SELECT {RECORD_COLUMNS} FROM usage_records ")),
        "the read-back selects the whole read list: {batch}"
    );
}
