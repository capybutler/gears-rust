//! The one independent oracle for the ledger table's own migrations —
//! `migrations/0001_init.sql`, which declares it. There is no second
//! migration to parse: `0002_usage_rollup.sql` creates a continuous aggregate
//! and touches no column of the ledger table. A later migration that does add
//! a column takes its own constant here beside `MIGRATION_SQL`.
//!
//! Shared rather than duplicated: the constants in this crate that spell the
//! ledger's column sequence are checked against *this* parse and never against
//! a second hand transcription, which would be a second thing to keep and
//! would drift silently.
//!
//! **One link in that chain is out of reach from here.** `InsertColumns`' field
//! order reaches the DDL only through the `.bind()` sequence in
//! `record_store.rs`, and nothing in-process observes that sequence: swap two
//! binds of the same SQL type — `resource_id` and `resource_type` are both
//! `text` and both `NOT NULL` — and Postgres accepts the row while every
//! constant this parse checks is still correct. Closing it needs a live backend
//! writing a row and reading it back, which the `records_ingest_integration_pg`
//! column-for-column read-back does.
//!
//! Gated on `any(test, feature = "postgres")` rather than `test` alone so the
//! `tests/*.rs` integration crates can call it too. `postgres` is a test-only
//! feature, so nothing ships with this compiled in, and the wider gate is what
//! keeps "one independent oracle" literally true instead of becoming one parser
//! per compilation unit.
//!
//! Widening the gate also takes this module out of clippy's "is a test"
//! heuristic, which keys on a bare `#[cfg(test)]` — so `allow-expect-in-tests`
//! no longer covers it and [`ledger_columns`]' two `expect`s are exempted
//! explicitly. They are the right failure: a migration this parser cannot read
//! must abort loudly, because the alternative — an `Option` a caller can
//! `unwrap_or_default()` into an empty column list — is exactly the silent
//! vanishing every assertion here exists to prevent.

/// The schema itself, so nothing that names its columns can drift from it. The
/// path resolves from this file's own directory, which is the real one even
/// when a scratch harness `#[path]`-includes this module.
const MIGRATION_SQL: &str = include_str!("../../../migrations/0001_init.sql");

/// The ledger table's columns as declared — `(name, type)`, in declaration
/// order.
///
/// It recognizes what a column is **not**, rather than allowlisting type names.
/// The table-constraint keywords are closed by the SQL grammar and are upper
/// case; type names are open-ended, and an allowlist of them fails in the worse
/// direction — a column whose type is not on it vanishes from the parsed set,
/// so a developer who forgets to add it to a dependent constant gets a green
/// run and a blind guard. Here an unrecognized construct becomes an *extra*
/// entry and reds the test instead, which is the loud failure.
///
/// A column line is indented exactly four spaces, names an all-lower-case
/// identifier, and has a second token after it — which drops the `--` comments,
/// the upper-case `PRIMARY KEY`/`CONSTRAINT` lines, and the more deeply
/// indented constraint bodies. Every column in this table spells its type as a
/// single word, so `NOT NULL`, `DEFAULT …` and `CHECK …` all fall after it; a
/// parenthesized type carrying a space (`numeric(38, 9)`) would split across
/// tokens and red the pairing test rather than pass a wrong type silently.
#[expect(
    clippy::expect_used,
    reason = "a migration this parser cannot read must abort loudly; an Option a caller \
              can unwrap_or_default() into an empty column list is the silent vanishing \
              this module exists to prevent"
)]
#[must_use]
pub fn ledger_columns() -> Vec<(&'static str, &'static str)> {
    let start = MIGRATION_SQL
        .find("CREATE TABLE IF NOT EXISTS usage_records (")
        .expect("the ledger table is declared");
    let block = &MIGRATION_SQL[start..];
    let end = block.find("\n);").expect("the declaration is closed");
    let columns: Vec<(&'static str, &'static str)> = block[..end]
        .lines()
        .filter_map(|line| {
            let decl = line.strip_prefix("    ")?;
            if decl.starts_with(' ') {
                return None;
            }
            let mut parts = decl.split_whitespace();
            let name = parts.next()?;
            // A column declaration always has a type after the name.
            let ty = parts.next()?.trim_end_matches(',');
            let is_column = name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            is_column.then_some((name, ty))
        })
        .collect();
    columns
}

/// The columns of [`ledger_columns`] that an `INSERT` writes, `(name, type)` in
/// declaration order.
///
/// The exclusion is named here and **not** derived from `INSERT_COLUMNS`,
/// which is one of the constants this oracle exists to check: deriving it
/// from the code under test would let a column dropped from `INSERT_COLUMNS`
/// disappear from the expectation with it. `xact_id` is stamped by its column
/// default `pg_current_xact_id()` and never bound by the Record Store, which
/// is what makes every entry of one batch share one value; every other column,
/// `metadata`'s own default and `entry_type`'s enum type notwithstanding, is
/// supplied per row.
#[must_use]
pub fn insertable_columns() -> Vec<(&'static str, &'static str)> {
    ledger_columns()
        .into_iter()
        .filter(|(name, _)| *name != "xact_id")
        .collect()
}
