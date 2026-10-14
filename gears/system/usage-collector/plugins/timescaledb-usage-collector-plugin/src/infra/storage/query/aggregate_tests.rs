//! Unit tests for the aggregation SQL builders. Pure (no DB): they pin the
//! exact SQL each [`AggregationFold`] and [`AggregationDimension`] emits, that
//! the one caller-derived value is bound rather than interpolated, and that
//! every fragment qualifies its columns with the alias the `FROM` clause
//! declares.
//!
//! The alias guard's subject is **the assembled statement**, not this module:
//! it is fed the shared builders in [`super::super`] as well, because a
//! statement holding one qualified and one unqualified reference to the same
//! column is exactly the drift the shared `FROM` constant exists to stop, and
//! splitting the guard by which file emitted a fragment would let that through.
//!
//! The second half of this module is the statement those fragments compose
//! into — [`build_aggregate_sql`]. Its assertions are over the assembled text
//! and the bind vector, which are halves of one oracle: the text reads
//! identically no matter which value lands in which placeholder.

use toolkit_odata::{CursorV1, ODataQuery, SortDir};
use usage_collector_sdk::v2::{
    AggregationDimension, AggregationFold, MAX_AGGREGATION_BUCKETS, MetadataFilter, MetadataKey,
    TimeRange,
};
use uuid::Uuid;

use super::{AggregateStatement, build_aggregate_sql};
use crate::infra::storage::migration_probe;
use crate::infra::storage::test_support::{
    RANGE_FROM_UNIX, RANGE_TO_UNIX, READ_FINGERPRINT, SCOPE_TENANT_A, SCOPE_TENANT_B, every_fold,
    fold_query, list_meter, list_range, parse_scope,
};

use super::super::bind::SqlBind;
use super::super::translate::SqlCtx;
use super::super::{
    ledger_from_clause, push_metadata_filter_clauses, push_meter_and_range_clauses,
};
use super::{
    aggregate_limit_clause, dimension_presence_guard, dimension_select_expr, fold_select_expr,
    withdrawal_exclusion_clause,
};

#[test]
fn every_fold_casts_to_numeric() {
    // `SUM` is the one arm with a `COALESCE`: `PostgreSQL` answers `NULL` over
    // zero rows and DESIGN 3.3 fixes an empty selection's `SUM` at 0.
    assert_eq!(
        fold_select_expr(AggregationFold::Sum),
        Some("COALESCE(SUM(r.quantity), 0)::numeric")
    );
    assert_eq!(
        fold_select_expr(AggregationFold::Count),
        Some("COUNT(*)::numeric")
    );
    assert_eq!(
        fold_select_expr(AggregationFold::Min),
        Some("MIN(r.quantity)::numeric")
    );
    assert_eq!(
        fold_select_expr(AggregationFold::Max),
        Some("MAX(r.quantity)::numeric")
    );
    // `LATEST` is an ordered pick rather than an aggregate function, but it
    // casts like the rest so every fold reads back as `Option<BigDecimal>`.
    assert!(
        fold_select_expr(AggregationFold::Latest)
            .expect("LATEST resolves")
            .ends_with("::numeric")
    );
}

#[test]
fn latest_picks_the_greatest_window_end_then_accepted_at_then_id() {
    // The gear's DESIGN 3.1 rule, key for key: greatest `window_end`, then
    // greatest `accepted_at`, then greatest `id` in byte order. All three keys
    // are DESC and in that order; `[1]` takes the head of the ordered array.
    // `id` is unique, so the order is total and holds across a group spanning
    // tenants -- which is why the third key is not decoration. This plugin's
    // own DESIGN 3.6 states the same rule as its own.
    //
    // Pinning the whole expression as a literal, rather than asserting that it
    // contains each key, is what makes a reordering visible: `accepted_at DESC,
    // window_end DESC` contains all three keys and answers a different fold.
    //
    // The SDK's `latest-tie-break` check is what holds this to the rule against
    // a live backend, over a pair whose `id` order and arrival order both
    // disagree with `accepted_at`. This assertion is the in-process half: it
    // reddens without a container.
    assert_eq!(
        fold_select_expr(AggregationFold::Latest),
        Some(
            "(ARRAY_AGG(r.quantity ORDER BY r.window_end DESC, r.accepted_at DESC, \
             r.id DESC))[1]::numeric"
        )
    );
}

// ── withdrawal exclusion ─────────────────────────────────────────────────────
//
// Two obligations, two tests. Each conjunct of the clause implements one of
// the SPI's two independently-stated obligations, so deleting either must red
// a test of its own rather than ride on the other's coverage.
//
// Neither test loops over `AggregationFold`: the clause takes no fold, so
// fold-independence is a property of the signature and a reintroduced per-fold
// branch fails to compile here rather than failing an assertion.

#[test]
fn an_invalidation_entry_is_excluded_under_every_fold() {
    // Obligation 1: an invalidation entry contributes nothing to any fold,
    // whether or not its target is in the selection. It stands alone because
    // retention is plugin-owned, so a conforming deployment can purge a target
    // and keep the invalidation that withdrew it; that orphan is the echoed
    // quantity with nothing left to pair it against.
    assert!(
        withdrawal_exclusion_clause().contains("r.invalidates IS NULL"),
        "an invalidation entry must contribute nothing to any fold, got {}",
        withdrawal_exclusion_clause()
    );
}

#[test]
fn a_record_an_accepted_invalidation_names_is_excluded_under_every_fold() {
    // Obligation 2: the entry an accepted invalidation names contributes
    // nothing either. Leaving out only the invalidation double-counts the
    // measurement the withdrawal was meant to remove, because an invalidation
    // echoes the quantity it withdraws rather than negating it.
    assert!(
        withdrawal_exclusion_clause().contains(
            "NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id AND w.type_key = r.type_key)"
        ),
        "the entry an accepted invalidation names must contribute nothing either, got {}",
        withdrawal_exclusion_clause()
    );
}

#[test]
fn no_fold_gets_its_own_withdrawal_rule() {
    // The retired `corrects_id` model made `SUM` the exception: it netted
    // across signed compensation rows and so deliberately did not filter them.
    // Under the append-only model an invalidation echoes rather than negates,
    // so netting double-counts and one rule holds under every fold. This pins
    // that the clause is whole, not a fragment one fold may skip.
    assert_eq!(
        withdrawal_exclusion_clause(),
        "r.invalidates IS NULL \
         AND NOT EXISTS (SELECT 1 FROM usage_records w \
         WHERE w.invalidates = r.id AND w.type_key = r.type_key)"
    );
}

#[test]
fn limit_clause_caps_grouped_queries_at_cap_plus_one() {
    // The `+ 1` is load-bearing: it lets the gateway distinguish a result
    // exactly at the cap (allowed) from one over it (rejected 400).
    let expected = format!(" LIMIT {}", MAX_AGGREGATION_BUCKETS + 1);
    assert_eq!(aggregate_limit_clause(1), expected);
    assert_eq!(aggregate_limit_clause(2), expected);
}

#[test]
fn limit_clause_absent_for_no_grouping() {
    // No `group_by` → a single aggregate row; no cardinality to bound.
    assert_eq!(aggregate_limit_clause(0), "");
}

// ── dimension select-expr ────────────────────────────────────────────────────

#[test]
fn metadata_dimension_binds_the_key_and_emits_json_extract() {
    // The one aggregation builder that touches caller-derived input: the
    // metadata dimension key MUST be bound (`r.metadata ->> $N`), never
    // interpolated. A regression that inlined the key would surface here as a
    // changed expr string and/or a missing bind.
    let dim = AggregationDimension::Metadata(MetadataKey::new("region").unwrap());
    let mut ctx = SqlCtx::new(1);
    let expr = dimension_select_expr(&dim, &mut ctx);
    assert_eq!(expr.as_deref(), Some("r.metadata ->> $1"));
    assert_eq!(ctx.binds.len(), 1, "exactly one bind is pushed");
    assert!(
        matches!(&ctx.binds[0], SqlBind::Str(s) if s == "region"),
        "the caller-derived key is bound verbatim as text, got {:?}",
        ctx.binds[0]
    );
}

#[test]
fn metadata_dimension_placeholder_honors_start_offset() {
    // The placeholder index comes from `ctx`, not a hardcoded `$1`, so the
    // dimension expr composes correctly after leading binds (e.g. the meter
    // scope at `$1`).
    let dim = AggregationDimension::Metadata(MetadataKey::new("tier").unwrap());
    let mut ctx = SqlCtx::new(3);
    assert_eq!(
        dimension_select_expr(&dim, &mut ctx).as_deref(),
        Some("r.metadata ->> $3")
    );
}

#[test]
fn identity_dimensions_emit_static_columns_and_bind_nothing() {
    // Every non-metadata dimension is a closed-enum `'static` column (an
    // allowlist), so none may push a bind.
    for (dim, expected) in [
        (AggregationDimension::TenantId, "r.tenant_id::text"),
        (AggregationDimension::ResourceId, "r.resource_id"),
        (AggregationDimension::ResourceType, "r.resource_type"),
        (AggregationDimension::SubjectId, "r.subject_id"),
        (AggregationDimension::SubjectType, "r.subject_type"),
    ] {
        let mut ctx = SqlCtx::new(1);
        assert_eq!(
            dimension_select_expr(&dim, &mut ctx).as_deref(),
            Some(expected),
            "{dim:?}"
        );
        assert!(ctx.binds.is_empty(), "{dim:?} must bind nothing");
    }
}

// ── alias coupling ───────────────────────────────────────────────────────────

/// The ledger table's full column set, in declaration order.
/// [`ledger_columns_are_the_migrations_columns`] holds it to the ledger's own
/// migrations, so it is the schema's columns and not the subset
/// today's fragments happen to name. That difference is the point of the guard:
/// an arm added later reaches for a column no fragment mentions yet, and
/// `origin` is the likeliest one to be added next. Every
/// occurrence of any of these must carry a table qualifier, or the fragment
/// binds to whatever the caller's `FROM` happens to expose.
const LEDGER_COLUMNS: &[&str] = &[
    "id",
    "tenant_id",
    "gts_type_uuid",
    "type_key",
    "quantity",
    "window_start",
    "window_end",
    "resource_id",
    "resource_type",
    "subject_id",
    "subject_type",
    "idempotency_key",
    "invalidates",
    "reason_code",
    "origin",
    "entry_type",
    "accepted_at",
    "xact_id",
    "metadata",
];

/// The ledger table's column names as declared, in declaration order — the
/// name half of [`migration_probe::ledger_columns`], the crate's one parse of
/// the ledger's migrations. The parser lives there rather than here because
/// `record_store_tests` checks the insert-side constants against the same
/// parse; a second transcription of the schema is exactly what it exists to
/// remove.
fn migration_ledger_columns() -> Vec<&'static str> {
    migration_probe::ledger_columns()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn ledger_columns_are_the_migrations_columns() {
    // Without this the constant is a hand-kept list, and the alias guard below
    // is silent for exactly the columns someone forgot to add to it -- which is
    // the defect this whole section exists to close. Order is asserted too, so
    // the constant stays readable against the schema.
    assert_eq!(
        migration_ledger_columns().as_slice(),
        LEDGER_COLUMNS,
        "LEDGER_COLUMNS must be the migrations' usage_records columns, in order"
    );
}

/// The aliases `sql` may qualify a column with. `r`, the alias
/// [`ledger_from_clause`] declares, is always admissible. `w` and `k` are
/// admissible only in a fragment that opens them — the withdrawal subquery's
/// second ledger and the type-key lookup's `usage_type_key` — so a fragment
/// borrowing either without opening it is an offender rather than a pass.
fn aliases_for(sql: &str) -> &'static [u8] {
    match (
        sql.contains("FROM usage_records w"),
        sql.contains("FROM usage_type_key k"),
    ) {
        (true, true) => b"rwk",
        (true, false) => b"rw",
        (false, true) => b"rk",
        (false, false) => b"r",
    }
}

/// Whether the ledger column starting at `at` carries one of the aliases
/// [`aliases_for`] admits. A bare column, or one qualified with any other
/// alias, is false.
fn is_qualified(sql: &str, at: usize) -> bool {
    let b = sql.as_bytes();
    at >= 2
        && b[at - 1] == b'.'
        && aliases_for(sql).contains(&b[at - 2])
        && (at == 2 || !(b[at - 3].is_ascii_alphanumeric() || b[at - 3] == b'_'))
}

/// Every ledger column in `sql` that is not qualified with an alias
/// [`aliases_for`] admits. Splits on identifier boundaries so `subject_id` is
/// one token rather than a `subject_type` plus an `id`.
fn misqualified_columns(sql: &str) -> Vec<&'static str> {
    let mut offenders = Vec::new();
    let mut start = 0;
    let check = |token: &str, start: usize, offenders: &mut Vec<&'static str>| {
        if let Some(col) = LEDGER_COLUMNS.iter().find(|c| **c == token)
            && !is_qualified(sql, start)
        {
            offenders.push(*col);
        }
    };
    for (i, ch) in sql.char_indices() {
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            if start < i {
                check(&sql[start..i], start, &mut offenders);
            }
            start = i + ch.len_utf8();
        }
    }
    if start < sql.len() {
        check(&sql[start..], start, &mut offenders);
    }
    offenders
}

/// Every fragment an assembled ledger statement is built from that can name a
/// column — this module's, all but [`aggregate_limit_clause`] whose output is a
/// row count, plus the builders both read paths share.
///
/// `translate_scope`'s output is the one fragment of an assembled statement
/// that is **not** here, and it is the one that does not qualify:
/// [`record_column`](super::super::translate::record_column) returns bare
/// column names, so a translated `$filter` reads `tenant_id = $4`. That is
/// correct SQL — one table is in scope in the outer `WHERE`, so the alias is
/// implicit — and it is shared with the list path rather than this one's.
/// `RECORD_COLUMNS` in the list `SELECT` is bare for the same reason.
///
/// The two `match` statements are exhaustiveness witnesses and nothing else:
/// adding a variant to either enum fails to compile *here*, next to the array
/// that needs its new entry. Without them a new dimension arm reds only
/// `dimension_select_expr`'s own match, and the alias guard below would go on
/// reporting a coverage it had silently stopped having.
fn all_fragments() -> Vec<String> {
    let mut ctx = SqlCtx::new(1);
    let mut fragments: Vec<String> = vec![
        ledger_from_clause().to_owned(),
        withdrawal_exclusion_clause().to_owned(),
    ];
    // The shared builders push into a clause vector rather than returning one
    // fragment, so drain them into the same list the per-fragment guard walks.
    push_meter_and_range_clauses(
        Uuid::from_u128(0x5ee1_0000_0000_0001),
        TimeRange::new(
            time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid ts"),
            time::OffsetDateTime::from_unix_timestamp(1_700_003_600).expect("valid ts"),
        )
        .expect("a strictly ordered range"),
        &mut ctx,
        &mut fragments,
    );
    push_metadata_filter_clauses(
        &[MetadataFilter::new("region", ["eu-west-1"]).expect("valid metadata filter")],
        &mut ctx,
        &mut fragments,
    );
    for fold in AggregationFold::ALL {
        fragments.push(
            fold_select_expr(*fold)
                .expect("every published fold resolves")
                .to_owned(),
        );
    }
    for dim in every_dimension() {
        let expr =
            dimension_select_expr(&dim, &mut ctx).expect("every published dimension resolves");
        // The `WHERE` half of the same dimension. It is derived from `expr`, so
        // it inherits the alias qualification rather than choosing one — but
        // walking it anyway is what keeps that a fact about the code instead of
        // a fact about how it happens to be written today.
        if let Some(guard) = dimension_presence_guard(&dim, &expr) {
            fragments.push(guard);
        }
        fragments.push(expr);
    }
    fragments
}

#[test]
fn every_fragment_qualifies_its_columns_with_the_alias_the_from_clause_declares() {
    // The alias is one constant every read path reads (`ledger_from_clause`),
    // but the fragments still hard-code `r` in their text, so the two can
    // drift. This is the half of that coupling a pure test can pin: no fragment
    // may name a bare column, and none may reach for an alias it did not open,
    // so a fragment drifting off `r` is caught here rather than by a query
    // failing against a live database.
    assert!(
        ledger_from_clause().ends_with(" r"),
        "the FROM clause must declare the `r` every other fragment binds to, got {}",
        ledger_from_clause()
    );
    for fragment in all_fragments() {
        assert!(
            misqualified_columns(&fragment).is_empty(),
            "{fragment} names {:?} without the alias the FROM clause declares",
            misqualified_columns(&fragment)
        );
    }
}

// --- The assembled statement: `build_aggregate_sql` ---

/// The metadata key a grouped metadata dimension reads, distinct from the side
/// channel's `region`, so a statement carrying both cannot pass by binding one
/// key where the other belongs.
const GROUPED_METADATA_KEY: &str = "tier";

fn metadata_dimension(key: &str) -> AggregationDimension {
    AggregationDimension::Metadata(MetadataKey::new(key).expect("a valid metadata key"))
}

/// Every dimension the SDK declares — the witness for a test that claims to
/// cover "every dimension".
///
/// Read off [`AggregationDimension::ALL_UNIT_VARIANTS`] rather than listed
/// here: `AggregationDimension` is `#[non_exhaustive]`, so an out-of-crate
/// `match` over it needs a wildcard arm and is no witness at all. A unit
/// dimension the SDK adds therefore lands in this set on its own, and
/// `every_dimension_resolves_to_an_expression` is what reddens for it.
/// `Metadata` is appended by hand because it carries a key.
fn every_dimension() -> Vec<AggregationDimension> {
    let mut dims = AggregationDimension::ALL_UNIT_VARIANTS.to_vec();
    dims.push(metadata_dimension(GROUPED_METADATA_KEY));
    dims
}

/// Every dimension resolves to a SELECT expression — the run-time half of what
/// the removed exhaustive `match` used to give at compile time.
#[test]
fn every_dimension_resolves_to_an_expression() {
    for dim in every_dimension() {
        let mut ctx = SqlCtx::new(1);
        assert!(
            dimension_select_expr(&dim, &mut ctx).is_some(),
            "{dim:?} has no GROUP BY expression; a dimension added to the SDK needs an arm \
             in dimension_select_expr and a NULL decision in dimension_presence_guard"
        );
    }
}

/// Every fold resolves to a SELECT expression, for [`every_dimension`]'s reason.
#[test]
fn every_fold_resolves_to_an_expression() {
    for fold in AggregationFold::ALL {
        assert!(
            fold_select_expr(*fold).is_some(),
            "{fold:?} has no fold expression; a fold added to the SDK needs an arm in \
             fold_select_expr"
        );
    }
}
/// One grouped statement carrying every moving part at once: the meter and range,
/// the withdrawal exclusion, a two-conjunct compiled scope, a side-channel filter,
/// and two grouped dimensions — one of them the metadata escape hatch, whose key
/// is bound and whose presence guard reads the same placeholder.
///
/// Two tests read it, because the statement text and the bind vector are halves of
/// one oracle and the text is the weaker half: it reads identically no matter
/// which value lands in which placeholder.
fn full_fold_statement() -> AggregateStatement {
    let query = fold_query().with_filter(parse_scope(&format!(
        "tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm'"
    )));
    let group_by = vec![
        AggregationDimension::SubjectType,
        metadata_dimension(GROUPED_METADATA_KEY),
    ];

    build_aggregate_sql(
        list_meter(),
        list_range(),
        AggregationFold::Sum,
        &query,
        &[MetadataFilter::new("region", ["eu-west-1"]).expect("a valid side channel filter")],
        &group_by,
    )
    .expect("a well-formed fold must render")
}

#[test]
fn the_fold_assembles_one_statement_from_the_builders_both_read_paths_share() {
    let sql = full_fold_statement().sql;

    // Transcribed by hand, from the shape the SPI describes rather than from
    // anything a builder produces — including the `100001`, which is
    // `MAX_AGGREGATION_BUCKETS + 1` written out. `aggregate_tests.rs` pins the
    // clause against the constant; if this line derived the number from the
    // same constant the two layers would only be agreeing with each other.
    assert_eq!(
        sql,
        "SELECT r.subject_type, r.metadata ->> $8, COALESCE(SUM(r.quantity), 0)::numeric \
         FROM usage_records r \
         WHERE r.gts_type_uuid = $1 \
         AND r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_uuid = $1) \
         AND r.window_end >= $2 AND r.window_end < $3 \
         AND r.invalidates IS NULL \
         AND NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id AND w.type_key = r.type_key) \
         AND ((tenant_id = $4 AND resource_type = $5)) \
         AND r.metadata ->> $6 IN ($7) \
         AND r.subject_type IS NOT NULL \
         AND r.metadata ->> $8 IS NOT NULL \
         GROUP BY 1, 2 LIMIT 100001"
    );
}

#[test]
fn the_fold_binds_the_meter_and_both_range_bounds_ahead_of_everything() {
    // The statement text is the weaker half of the oracle: it reads identically
    // however the values are permuted across its placeholders, and a wrong total
    // is a billing figure that looks fine. `query_tests.rs` pins what
    // `push_meter_and_range_clauses` binds in isolation; what is this builder's is
    // that it calls that builder first, so those binds lead.
    let binds = full_fold_statement().binds;

    assert!(
        matches!(&binds[0], SqlBind::Uuid(u) if *u == list_meter()),
        "$1 is the meter this fold reads; an empty string here would drop the \
         scoping to one meter without changing a character of the SQL. got: {:?}",
        binds[0]
    );
    assert!(
        matches!(&binds[1], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_FROM_UNIX),
        "$2 is the range's inclusive lower bound. got: {:?}",
        binds[1]
    );
    assert!(
        matches!(&binds[2], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_TO_UNIX),
        "$3 is the range's exclusive upper bound; swapped with $2 the range \
         inverts to `window_end >= to AND window_end < from` and the fold \
         selects nothing, ever. got: {:?}",
        binds[2]
    );
}

#[test]
fn the_fold_binds_its_callers_values_behind_the_range() {
    // The other half of the bind oracle: the compiled scope's two operands, the
    // side channel's key and value, and the grouped metadata key — each pinned
    // by value in placeholder order, with the length assertion a completeness
    // check on that set rather than the oracle itself. All five are `text` or
    // `uuid` binds into a statement whose text cannot tell them apart.
    let binds = full_fold_statement().binds;

    assert_eq!(
        binds.len(),
        8,
        "the meter, both bounds, both scope operands, the side channel key and \
         its one value, and the grouped metadata key. got: {binds:?}"
    );
    assert!(
        matches!(&binds[3], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_A),
        "$4 is the tenant the compiled scope pins. got: {:?}",
        binds[3]
    );
    assert!(
        matches!(&binds[4], SqlBind::Str(s) if s == "vm"),
        "$5 is the resource type the compiled scope pins. got: {:?}",
        binds[4]
    );
    assert!(
        matches!(&binds[5], SqlBind::Str(s) if s == "region"),
        "$6 is the side channel's key. got: {:?}",
        binds[5]
    );
    assert!(
        matches!(&binds[6], SqlBind::Str(s) if s == "eu-west-1"),
        "$7 is the side channel's one value; bound where the key belongs, the \
         fold reads a different facet of every row. got: {:?}",
        binds[6]
    );
    assert!(
        matches!(&binds[7], SqlBind::Str(s) if s == GROUPED_METADATA_KEY),
        "$8 is the grouped metadata key, bound once and read twice: by the \
         SELECT expression and by the presence guard. got: {:?}",
        binds[7]
    );
}

#[test]
fn the_folds_from_clause_is_the_one_constant_both_read_paths_read() {
    let AggregateStatement { sql, .. } = build_aggregate_sql(
        list_meter(),
        list_range(),
        AggregationFold::Sum,
        &fold_query(),
        &[],
        &[],
    )
    .expect("a well-formed fold must render");

    // Called, never spelled. Every fragment in the statement qualifies its columns
    // with the alias this clause declares, and the withdrawal exclusion's
    // `w.invalidates = r.id` is unresolvable without it — an unaliased
    // `FROM usage_records` beside an alias-qualified fragment is a statement
    // `PostgreSQL` refuses outright.
    assert!(
        sql.contains(ledger_from_clause()),
        "the fold's FROM must be the shared clause. got: {sql}"
    );
}

#[test]
fn no_fold_escapes_the_withdrawal_exclusion() {
    // `aggregate_tests.rs` pins that the clause has no per-fold branch; this
    // pins that the statement builder applies it at all, under each fold in
    // turn. Deleting the one `clauses.push` reds every arm of this loop.
    for fold in every_fold() {
        let sql = build_aggregate_sql(list_meter(), list_range(), fold, &fold_query(), &[], &[])
            .expect("a well-formed fold must render")
            .sql;

        assert!(
            sql.contains(withdrawal_exclusion_clause()),
            "an invalidation entry, and the record an accepted invalidation \
             names, contribute nothing under {fold:?} as under every other \
             fold. got: {sql}"
        );
        assert!(
            sql.contains(fold_select_expr(fold).expect("every published fold resolves")),
            "the statement must fold with the fold it was asked for. got: {sql}"
        );
    }
}

#[test]
fn the_fold_carries_no_status_predicate() {
    // The trap of a "column X is absent" assertion matching the SELECT list
    // instead of the WHERE clause does not reach this statement: an aggregate
    // SELECT list is the grouped dimension expressions followed by the fold
    // expression, never `RECORD_COLUMNS`, and neither names `status`. So the whole
    // statement is the right thing to grep, and grouping by every dimension at
    // once is the widest text this builder can produce.
    for fold in every_fold() {
        let AggregateStatement { sql, .. } = build_aggregate_sql(
            list_meter(),
            list_range(),
            fold,
            &fold_query(),
            &[MetadataFilter::new("region", ["eu-west-1"]).expect("a valid filter")],
            &every_dimension(),
        )
        .expect("a well-formed fold must render");

        assert!(
            !sql.contains("status"),
            "`status` is not a column in this schema. got: {sql}"
        );
    }
}

#[test]
fn the_no_grouping_case_is_one_bare_aggregate_row() {
    // The single empty-keyed bucket a conforming plugin owes for an empty
    // `group_by` is `PostgreSQL`'s own answer to a bare aggregate — exactly one
    // row, and `Some(0)` rather than `None` for `COUNT`. That only holds while the
    // statement stays a bare aggregate, so this kills both emitting the `GROUP BY`
    // unconditionally (an empty ordinal list is a syntax error, and a `1` bolted on
    // groups by the fold itself) and emitting the bucket `LIMIT` with no group
    // cardinality to bound.
    let AggregateStatement { sql, binds, .. } = build_aggregate_sql(
        list_meter(),
        list_range(),
        AggregationFold::Count,
        &fold_query(),
        &[],
        &[],
    )
    .expect("an ungrouped fold must render");

    assert!(
        !sql.contains("GROUP BY"),
        "grouping by nothing is what a bare aggregate already does. got: {sql}"
    );
    assert!(
        !sql.contains("LIMIT"),
        "there is one row and no group cardinality to bound. got: {sql}"
    );
    assert!(
        sql.starts_with("SELECT COUNT(*)::numeric FROM "),
        "the SELECT list is the fold alone. got: {sql}"
    );
    // The ungrouped statement binds the meter and the two bounds and nothing
    // else, so a stray dimension bind would show up here as a fourth value.
    assert_eq!(
        binds.len(),
        3,
        "the meter and the two bounds. got: {binds:?}"
    );
    assert!(
        matches!(&binds[0], SqlBind::Uuid(u) if *u == list_meter()),
        "$1 is the meter. got: {:?}",
        binds[0]
    );
    assert!(
        matches!(&binds[1], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_FROM_UNIX),
        "$2 is the inclusive lower bound. got: {:?}",
        binds[1]
    );
    assert!(
        matches!(&binds[2], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_TO_UNIX),
        "$3 is the exclusive upper bound. got: {:?}",
        binds[2]
    );
}

#[test]
fn grouping_numbers_its_ordinals_from_one_and_bounds_the_bucket_count() {
    // `GROUP BY` by ordinal so a bound metadata expression is written once:
    // repeating the expression would push its key a second time and renumber
    // every placeholder after it. Ordinals are 1-based — a 0-based list is a
    // `PostgreSQL` error rather than a silently wrong grouping, but only for
    // the first ordinal, so the three-dimension case is what pins the shape.
    let AggregateStatement { sql, binds, .. } = build_aggregate_sql(
        list_meter(),
        list_range(),
        AggregationFold::Max,
        &fold_query(),
        &[],
        &[
            AggregationDimension::TenantId,
            AggregationDimension::ResourceId,
            metadata_dimension(GROUPED_METADATA_KEY),
        ],
    )
    .expect("a grouped fold must render");

    assert!(
        sql.ends_with(" GROUP BY 1, 2, 3 LIMIT 100001"),
        "three dimensions, then the cap plus one. got: {sql}"
    );
    assert!(
        sql.starts_with("SELECT r.tenant_id::text, r.resource_id, r.metadata ->> $4, MAX(r.quantity)::numeric FROM "),
        "the SELECT list is the dimensions in `group_by` order, then the fold. \
         got: {sql}"
    );
    assert_eq!(
        binds.len(),
        4,
        "the meter, both bounds, and the grouped metadata key once. got: {binds:?}"
    );
    assert!(
        matches!(&binds[3], SqlBind::Str(s) if s == GROUPED_METADATA_KEY),
        "$4 is the grouped metadata key. got: {:?}",
        binds[3]
    );
}

#[test]
fn the_builder_reports_the_dimension_count_its_select_list_was_built_from() {
    // `dim_count` travels with the statement so the decoder reads exactly as many
    // key columns as the SELECT list emits, rather than deriving the number again
    // from `group_by`. That buys nothing unless the reported count and the
    // statement agree — two separate outputs of one builder, and pinning the count
    // to a constant leaves every SQL oracle in this file green.
    //
    // Transcribed by hand: k dimensions means k leading key columns, `GROUP BY
    // 1..=k`, and `dim_count == k`. The empty case carries no `GROUP BY` at all,
    // which is why that assertion is two-way rather than a suffix match.
    for (group_by, count, tail) in [
        (vec![], 0usize, ""),
        (
            vec![AggregationDimension::TenantId],
            1,
            " GROUP BY 1 LIMIT 100001",
        ),
        (
            vec![
                AggregationDimension::TenantId,
                AggregationDimension::ResourceId,
                metadata_dimension(GROUPED_METADATA_KEY),
            ],
            3,
            " GROUP BY 1, 2, 3 LIMIT 100001",
        ),
    ] {
        let statement = build_aggregate_sql(
            list_meter(),
            list_range(),
            AggregationFold::Sum,
            &fold_query(),
            &[],
            &group_by,
        )
        .expect("a well-formed fold must render");

        assert_eq!(
            statement.dim_count, count,
            "the statement was built from {count} dimensions and must say so. \
             got: {}",
            statement.sql
        );
        assert!(
            statement.sql.ends_with(tail),
            "the GROUP BY must carry one ordinal per dimension. got: {}",
            statement.sql
        );
        assert_eq!(
            statement.sql.contains("GROUP BY"),
            count > 0,
            "grouping by nothing is what a bare aggregate already does. got: {}",
            statement.sql
        );
    }
}

#[test]
fn a_nullable_dimension_drops_the_rows_that_do_not_carry_it() {
    // The absent-dimension rule, uniform across every dimension that can be
    // `NULL`: drop the row rather than fold it into a `NULL` bucket. What says so
    // is the SDK — `AggregationDimension::SubjectId` / `SubjectType`
    // (`models.rs`) — and this crate's own `dimension_presence_guard`
    // (`aggregate.rs`) is what enforces it.
    //
    // The guard is spelled against the grouping expression itself, so it is the
    // exact negation of "this expression yields NULL". `?` would not be: for a key
    // present with JSON `null` it is true while `->>` is `NULL`, and it is the
    // `NULL` that decides the bucket. Each guard is transcribed by hand,
    // placeholder included.
    for (dim, guard) in [
        (AggregationDimension::SubjectId, "r.subject_id IS NOT NULL"),
        (
            AggregationDimension::SubjectType,
            "r.subject_type IS NOT NULL",
        ),
        (
            metadata_dimension(GROUPED_METADATA_KEY),
            "r.metadata ->> $4 IS NOT NULL",
        ),
    ] {
        let AggregateStatement { sql, .. } = build_aggregate_sql(
            list_meter(),
            list_range(),
            AggregationFold::Sum,
            &fold_query(),
            &[],
            std::slice::from_ref(&dim),
        )
        .expect("a grouped fold must render");

        assert!(
            sql.contains(guard),
            "{dim:?} can be NULL, so a row missing it must be dropped rather \
             than bucketed under NULL. got: {sql}"
        );
        // The other half, and the one that makes the guard unable to drift:
        // strip ` IS NOT NULL` off the guard and what is left must be the
        // grouping expression itself, the same bound placeholder and all.
        let grouping_expr = guard
            .strip_suffix(" IS NOT NULL")
            .unwrap_or_else(|| unreachable!("the transcribed guard must end in the predicate"));
        assert!(
            sql.starts_with(&format!("SELECT {grouping_expr}, ")),
            "the guard must negate the grouping expression it guards, not a \
             second spelling of it. got: {sql}"
        );
        assert!(
            !sql.contains(" ? "),
            "the guard is `->> IS NOT NULL`, never the containment operator: \
             they disagree on a key present with JSON null, and a bare `?` \
             collides with placeholder syntax in some drivers. got: {sql}"
        );
    }
}

#[test]
fn the_three_never_null_dimensions_get_no_dead_guard() {
    // `tenant_id`, `resource_id` and `resource_type` are NOT NULL in the
    // schema, so a presence guard on them is dead SQL that the planner still
    // has to carry. The mutation this kills is the tempting uniform one: guard
    // every dimension because three of them need it.
    let AggregateStatement { sql, .. } = build_aggregate_sql(
        list_meter(),
        list_range(),
        AggregationFold::Sum,
        &fold_query(),
        &[],
        &[
            AggregationDimension::TenantId,
            AggregationDimension::ResourceId,
            AggregationDimension::ResourceType,
        ],
    )
    .expect("a grouped fold must render");

    assert!(
        !sql.contains("IS NOT NULL"),
        "none of these three columns can be NULL, so none needs a guard. \
         got: {sql}"
    );
}

#[test]
fn the_folds_disjunctive_scope_survives_the_conjunction_with_the_range() {
    // The shape a multi-constraint grant compiles to. Pushed into the
    // `join(" AND ")` unparenthesized, `… AND A OR B` binds as `(… AND A) OR B`
    // and folds every row matching the last disjunct, whatever meter or range was
    // asked for. Nothing at this layer can tell how many constraints the PDP
    // returned.
    let query = fold_query().with_filter(parse_scope(&format!(
        "(tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm') or \
         (tenant_id eq {SCOPE_TENANT_B} and resource_type eq 'vm')"
    )));

    let AggregateStatement { sql, binds, .. } = build_aggregate_sql(
        list_meter(),
        list_range(),
        AggregationFold::Sum,
        &query,
        &[],
        &[],
    )
    .expect("a compiled scope must render");

    // Transcribed by hand.
    assert!(
        sql.contains(
            "AND (((tenant_id = $4 AND resource_type = $5) \
             OR (tenant_id = $6 AND resource_type = $7)))"
        ),
        "the whole disjunction must sit inside the conjunction. got: {sql}"
    );
    assert_eq!(
        binds.len(),
        7,
        "the meter, both bounds, four scope operands"
    );
    assert!(
        matches!(&binds[3], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_A),
        "$4 is the first admitted tenant. got: {:?}",
        binds[3]
    );
    assert!(
        matches!(&binds[5], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_B),
        "$6 is the second admitted tenant; both grants must be bound, or the \
         scope silently narrows to one. got: {:?}",
        binds[5]
    );
}

#[test]
fn the_fold_reads_neither_the_cursor_nor_the_fingerprint_slot() {
    // "An aggregate implementation MUST NOT read the slot": this call paginates
    // nothing, mints no cursor, and the gateway assigns it no fingerprint. The
    // realistic mutation is the cursor block copied across from `build_list_sql`,
    // which reads `query.filter_hash` and pushes a keyset predicate — under that
    // copy the statements below stop matching each other.
    //
    // What this pins is that neither slot reaches the statement or its binds; a
    // read whose value is discarded leaves no trace in either.
    let base = fold_query();
    let with_cursor = fold_query().with_cursor(CursorV1 {
        k: vec!["2023-11-14T23:13:20Z".to_owned()],
        o: SortDir::Asc,
        s: "+window_end".to_owned(),
        f: None,
        d: "fwd".to_owned(),
    });
    let with_fingerprint = fold_query().with_filter_hash(READ_FINGERPRINT.to_owned());

    let render = |query: &ODataQuery| {
        let AggregateStatement { sql, binds, .. } = build_aggregate_sql(
            list_meter(),
            list_range(),
            AggregationFold::Sum,
            query,
            &[],
            &[AggregationDimension::TenantId],
        )
        .expect("a well-formed fold must render");
        (sql, format!("{binds:?}"))
    };

    let expected = render(&base);
    assert_eq!(
        render(&with_cursor),
        expected,
        "a cursor in the slot must change neither the statement nor its binds"
    );
    assert_eq!(
        render(&with_fingerprint),
        expected,
        "a fingerprint in the slot must change neither the statement nor its \
         binds"
    );
}

// `#[tokio::test]`, not `#[test]`: `lazy_store()`'s `connect_lazy` spawns the
// pool's background maintenance task, which needs a Tokio context — every
// other `lazy_store()` caller in this file is async for the same reason.
