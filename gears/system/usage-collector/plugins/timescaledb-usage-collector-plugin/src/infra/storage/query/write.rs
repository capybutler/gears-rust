//! The guarded insert statement, and the conflict read-back beside it.
//!
//! The single producer of both, on the principle [`super::feed`] and
//! [`super::keyset`] already follow: one function owns a path's SQL, so a
//! column name or a bind order cannot be stated twice and drift once. Every
//! column sequence these compose is [`super::super::columns`]', never a second
//! spelling.
//!
//! `DESIGN.md` §3.6's statement is `guarded_statement`, parameterized only in
//! its `input` source — the one thing a write path may vary — and
//! `BATCH_GUARDED_SQL` is that statement over `batch_input_source`, built
//! once because every input to it is a constant.
//!
//! What this module does **not** hold: the transaction the statement runs in,
//! the per-column pivot that binds it, and the decode of the verdicts it
//! returns. All three need a connection or the SDK model, and live beside it in
//! [`super::super::record_store::write`].

use std::sync::LazyLock;

use crate::infra::storage::columns::{
    DEDUP_CONFLICT_TARGET, INSERT_COLUMN_TYPES, INSERT_COLUMNS, PARTITION_PRUNE_COLUMN,
    RECORD_COLUMNS,
};

/// The `$n` the acceptance slack binds at: one past the last inserted column,
/// on both write paths.
///
/// Counted off [`INSERT_COLUMNS`] rather than written out, so the ledger
/// gaining or losing a written column moves it without anything being edited
/// twice.
pub(crate) fn slack_placeholder() -> usize {
    INSERT_COLUMNS.split(',').count() + 1
}

/// The admission guard `DESIGN.md` §3.6 requires **inside** the write
/// statement: the entry's `accepted_at` within `feed_acceptance_slack_secs` of
/// *that statement's own* `statement_timestamp()`, **in either direction**.
///
/// `abs(…)` is what makes it either direction. Dropped, an entry dated next year
/// is admitted; the sign flipped, the past-dated half goes unguarded.
/// `acceptance_slack_pg` carries one assertion per direction.
///
/// **One spelling, and the statement's own.** §3.6: *"Nothing before or after
/// the statement computes the guard, since a later statement would run under a
/// later `statement_timestamp()`."* It reads `t.accepted_at` off the `input`
/// relation rather than a placeholder, so the verdict is computed over the same
/// rows the INSERT selects from.
pub(crate) fn admitted_expr() -> String {
    format!(
        "(abs(extract(epoch FROM (t.accepted_at - statement_timestamp()))) <= ${}::bigint) \
         AS admitted",
        slack_placeholder()
    )
}

/// [`RECORD_COLUMNS`] as the outer select reads it back off `ins`.
///
/// The read list's one cast entry was already cast inside `ins`' `RETURNING`,
/// so `ins` exposes the **decoded** name — `entry_type` is a `text` column
/// there. Qualifying every name with `ins.` is what makes the not-won row's
/// columns `NULL` rather than silently picking up `input`'s like-named ones;
/// under `USING (id)` the merged `id` is the left side's and never null, so
/// `won` computed off an unqualified `id` would be true for every row.
pub(crate) fn ins_record_columns() -> String {
    RECORD_COLUMNS
        .split(',')
        .map(str::trim)
        .map(|entry| entry.rsplit_once(" AS ").map_or(entry, |(_, alias)| alias))
        .map(|name| format!("ins.{name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `DESIGN.md` §3.6's guarded write statement over `input_source`, which is the
/// **only** thing the two write paths differ in.
///
/// ```sql
/// WITH input AS (SELECT <INSERT_COLUMNS, metadata cast ::jsonb>, <admitted>
///                FROM <input_source>),
///      ins AS (INSERT INTO usage_records (<INSERT_COLUMNS>)
///              SELECT <INSERT_COLUMNS> FROM input WHERE admitted
///              ON CONFLICT (<DEDUP_CONFLICT_TARGET>) DO NOTHING
///              RETURNING <RECORD_COLUMNS>)
/// SELECT input.id AS input_id, input.admitted, (ins.id IS NOT NULL) AS won,
///        <RECORD_COLUMNS off ins>
/// FROM input LEFT JOIN ins USING (id)
/// ```
///
/// **`WHERE admitted` is what makes the verdict precede the identity.** A row
/// outside the slack never reaches `ON CONFLICT`, so it comes back
/// `admitted = false, won = false` whether or not its six inputs are already
/// stored, and the caller answers `Transient` without reading the ledger. §3.6:
/// *"a row not admitted is `Transient` … even when its identity exists"*.
///
/// **`input.id` is carried out** so the caller can align each returned row with
/// its input; `ins.id` is null on every row that did not win.
///
/// `xact_id` is bound nowhere and never read back here: the column default
/// stamps it from the inserting transaction, and only
/// [`FEED_COLUMNS`](crate::infra::storage::columns::FEED_COLUMNS)' one reader
/// needs the feed order's own key.
///
/// `input`'s `SELECT` list is [`input_select_list`]'s, so its columns are
/// [`INSERT_COLUMNS`]' names exactly and the inner `SELECT <INSERT_COLUMNS> FROM
/// input` cannot be a transposition of them.
// @cpt-algo:cpt-cf-uc-plugin-algo-guarded-insert-statement:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-single-entry-persistence:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-acceptance-slack-refusal:p1
pub(crate) fn guarded_statement(input_source: &str) -> String {
    format!(
        "WITH input AS (\
             SELECT {}, {} FROM {input_source}\
         ), \
         ins AS (\
             INSERT INTO usage_records ({INSERT_COLUMNS}) \
             SELECT {INSERT_COLUMNS} FROM input WHERE admitted \
             ON CONFLICT ({DEDUP_CONFLICT_TARGET}) DO NOTHING \
             RETURNING {RECORD_COLUMNS}\
         ) \
         SELECT input.id AS input_id, input.admitted, (ins.id IS NOT NULL) AS won, {} \
         FROM input LEFT JOIN ins USING (id)",
        input_select_list(),
        admitted_expr(),
        ins_record_columns(),
    )
}

/// [`INSERT_COLUMNS`] as the `input` CTE selects it: every column by its own
/// name, with `metadata` cast `::jsonb` and aliased back to itself.
///
/// **The cast is named rather than positional.** A trailing
/// `{INSERT_COLUMNS}::jsonb AS metadata` would bind the cast to whichever
/// identifier happens to be last, and breaks the moment the ledger declares a
/// column after `metadata`. Building the list from [`INSERT_COLUMNS`] itself,
/// one entry per name in its own order, keeps `input`'s columns exactly that
/// constant's names.
pub(crate) fn input_select_list() -> String {
    INSERT_COLUMNS
        .split(',')
        .map(str::trim)
        .map(|name| {
            if name == "metadata" {
                "metadata::jsonb AS metadata".to_owned()
            } else {
                name.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The batch path's `input` source: one array per column, `UNNEST`ed back into
/// rows and aliased to [`INSERT_COLUMNS`].
///
/// `sqlx` binds arrays, not rows. `UNNEST`'s parameters are
/// [`INSERT_COLUMN_TYPES`] in the same order, so `$n` is column `n`, and
/// the alias list is [`INSERT_COLUMNS`] itself, so the two cannot be transposed
/// relative to one another.
pub(crate) fn batch_input_source() -> String {
    let unnest = INSERT_COLUMN_TYPES
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("${}::{ty}[]", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    format!("UNNEST({unnest}) AS t({INSERT_COLUMNS})")
}

/// The batch conflict read-back: the rows that took these identities' slots.
///
/// **Two conjuncts, and neither is redundant in practice.** The row-value `IN`
/// pairs each `id` with its own period, which a second `= ANY` could not — that
/// would match one entry's `id` under another's period. The leading
/// `= ANY` is what the planner can exclude chunks with; it is logically implied
/// by the row-value and is therefore easy to mistake for dead weight.
///
/// Measured on `timescale/timescaledb:2.29.2-pg18`: without the `= ANY`
/// conjunct the `Append` lists every chunk of the ledger; with it the `Append`
/// lists only the time chunks the batch's periods fall in (times the number of
/// `type_key` slices, for the reason [`PARTITION_PRUNE_COLUMN`] gives). Dropping
/// the conjunct makes the read-back touch every retained chunk, inside the write
/// transaction and while it holds the batch's speculative tuple locks.
pub(crate) static BATCH_CONFLICT_READ_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {RECORD_COLUMNS} FROM usage_records \
         WHERE {PARTITION_PRUNE_COLUMN} = ANY($2::timestamptz[]) \
           AND (id, {PARTITION_PRUNE_COLUMN}) IN \
             (SELECT t1, t2 FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t(t1, t2))"
    )
});

/// The batch guarded statement ([`guarded_statement`] over
/// [`batch_input_source`]).
///
/// Built rather than inlined so a test can read the column list, the
/// placeholder count and the conflict target back out of it — and built
/// **once**, because every input to it is a constant and the alternative is one
/// `format!` per column of [`INSERT_COLUMNS`] on every write.
pub(crate) static BATCH_GUARDED_SQL: LazyLock<String> =
    LazyLock::new(|| guarded_statement(&batch_input_source()));

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "write_tests.rs"]
mod write_tests;
