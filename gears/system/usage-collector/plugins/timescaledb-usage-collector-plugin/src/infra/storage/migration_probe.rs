//! The one independent oracle for `migrations/0001_init.sql`.
//!
//! Shared rather than duplicated: the constants in this crate that spell the
//! ledger's column sequence — `LEDGER_COLUMNS`, `RECORD_COLUMNS`,
//! `INSERT_COLUMNS` and `INSERT_COLUMN_ARRAY_TYPES` — are checked against
//! *this* parse and never against a second hand transcription. A hand
//! transcription is a second thing to keep, and the two drift apart silently; a
//! parse of the shipped migration cannot, because it has nothing of its own to
//! forget.
//!
//! **One link in that chain is out of reach from here, and it is a real hole.**
//! `InsertColumns`' field order reaches the DDL only through the `.bind()`
//! sequence in `record_store.rs`, and nothing in-process observes that
//! sequence: swap two binds of the same SQL type — `resource_id` and
//! `resource_type` are both `text` and both `NOT NULL` — and Postgres accepts
//! the row, `InsertColumns::build`'s field-by-field test still passes, and this
//! parse says nothing, because every constant it checks is still correct.
//! Closing it needs a live backend writing a row and reading it back, which is
//! Task 15's. Do not read the list above as covering it.
//!
//! That makes this parser's quality the whole guarantee, which is why it
//! recognizes what a column is **not** rather than allowlisting type names —
//! see [`ledger_columns`].
//!
//! Gated on `any(test, feature = "postgres")` rather than `test` alone so the
//! five `tests/*.rs` integration crates can call it too. `postgres` is a
//! test-only feature, so nothing ships with this compiled in — and the point of
//! the wider gate is that "one independent oracle" stays literally true instead
//! of becoming one parser per compilation unit.
//!
//! Widening the gate also takes this module out of clippy's "is a test"
//! heuristic, which keys on a bare `#[cfg(test)]` — so `allow-expect-in-tests`
//! no longer covers it and the two `expect`s below are exempted explicitly.
//! They are the right failure: a migration this parser cannot read must abort
//! loudly, because the alternative — an `Option` a caller can
//! `unwrap_or_default()` into an empty column list — is exactly the silent
//! vanishing every assertion here exists to prevent.
//!
//! The two functions are `pub` because integration-test crates are external to
//! this one and `pub(crate)` would put them out of reach; under the `test`-only
//! half of the gate the module's own visibility caps them anyway, which is also
//! what `clippy::redundant_pub_crate` wants.

// Both `expect`s abort on a migration this parser cannot read. See the module
// doc: failing loudly is the design, and clippy's test exemption does not reach
// this module because the gate is not a bare `#[cfg(test)]`.
#![allow(clippy::expect_used)]

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
/// run and a blind guard, while one who remembers gets a red run telling a
/// correct edit it is wrong. Here an unrecognized construct becomes an *extra*
/// entry and reds the test instead, which is the loud failure.
///
/// A column line is indented exactly four spaces, names an all-lower-case
/// identifier, and has a second token after it. That drops the `--` comments,
/// the upper-case `PRIMARY KEY`/`CONSTRAINT` lines, and the more deeply
/// indented constraint bodies and generated-column continuations. The second
/// token is the declared type, with a trailing `,` stripped — a nullable
/// column ends its line at the type, so `subject_id text,` yields `text` and
/// not `text,`. Every column in this table spells its type as a single word, so
/// `NOT NULL`, `DEFAULT …`, `CHECK …` and `GENERATED ALWAYS …` all fall after
/// it; a parenthesized type carrying a space (`numeric(38, 9)`) would split
/// across tokens and red the pairing test rather than pass a wrong type
/// silently, which is the direction this parser fails in throughout.
#[must_use]
pub fn ledger_columns() -> Vec<(&'static str, &'static str)> {
    let start = MIGRATION_SQL
        .find("CREATE TABLE IF NOT EXISTS usage_records (")
        .expect("the ledger table is declared");
    let block = &MIGRATION_SQL[start..];
    let end = block.find("\n);").expect("the declaration is closed");
    block[..end]
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
        .collect()
}

/// The columns of [`ledger_columns`] that an `INSERT` writes, `(name, type)` in
/// declaration order.
///
/// The two exclusions are named here and **not** derived from `INSERT_COLUMNS`,
/// which is one of the constants this oracle exists to check: deriving them
/// from the code under test would let a column dropped from `INSERT_COLUMNS`
/// disappear from the expectation with it. `entry_type` is `GENERATED ALWAYS
/// … STORED` and `ingested_at` is `DEFAULT now()`, so the ledger writes both
/// itself; every other column, `metadata`'s own default notwithstanding, is
/// supplied per row.
#[must_use]
pub fn insertable_columns() -> Vec<(&'static str, &'static str)> {
    ledger_columns()
        .into_iter()
        .filter(|(name, _)| !matches!(*name, "entry_type" | "ingested_at"))
        .collect()
}
