//! The one independent oracle for `migrations/0001_init.sql`.
//!
//! Test-only support, shared rather than duplicated: every constant in this
//! crate that spells the ledger's column sequence — `LEDGER_COLUMNS`,
//! `RECORD_COLUMNS`, `INSERT_COLUMNS`, `INSERT_COLUMN_ARRAY_TYPES` and
//! `InsertColumns`' field order — is checked against *this* parse and never
//! against a second hand transcription. A hand transcription is a second thing
//! to keep, and the two drift apart silently; a parse of the shipped migration
//! cannot, because it has nothing of its own to forget.
//!
//! That makes this parser's quality the whole guarantee, which is why it
//! recognizes what a column is **not** rather than allowlisting type names —
//! see [`ledger_columns`].
//!
//! The two functions are `pub` rather than `pub(crate)` only because the module
//! is `pub(crate)`, which already caps them there; `pub(crate)` on top of that is
//! what `clippy::redundant_pub_crate` denies.

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
pub fn insertable_columns() -> Vec<(&'static str, &'static str)> {
    ledger_columns()
        .into_iter()
        .filter(|(name, _)| !matches!(*name, "entry_type" | "ingested_at"))
        .collect()
}
