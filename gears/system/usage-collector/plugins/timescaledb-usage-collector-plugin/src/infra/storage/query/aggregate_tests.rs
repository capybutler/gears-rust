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

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, MAX_AGGREGATION_BUCKETS, MetadataFilter, MetadataKey,
    MeterTypeId, TimeRange,
};

use crate::infra::storage::migration_probe;

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
    assert_eq!(
        fold_select_expr(AggregationFold::Sum),
        "SUM(r.value)::numeric"
    );
    assert_eq!(
        fold_select_expr(AggregationFold::Count),
        "COUNT(*)::numeric"
    );
    assert_eq!(
        fold_select_expr(AggregationFold::Min),
        "MIN(r.value)::numeric"
    );
    assert_eq!(
        fold_select_expr(AggregationFold::Max),
        "MAX(r.value)::numeric"
    );
    // `LATEST` is an ordered pick rather than an aggregate function, but it
    // casts like the rest so every fold reads back as `Option<BigDecimal>`.
    assert!(fold_select_expr(AggregationFold::Latest).ends_with("::numeric"));
}

#[test]
fn latest_picks_the_greatest_window_end_then_acceptance_sequence() {
    // DESIGN 3.1 declares the tie-break as greatest `window_end`, then
    // greatest `acceptance_sequence`. Both keys are DESC and in that order;
    // `[1]` takes the head of the ordered array. A backend substituting
    // another tie-break (the SDK reference plugin substitutes greatest `id`)
    // answers differently on the same ledger, and no contract check catches
    // it: `latest-tie-break` is in the SDK's `BLOCKED_CHECKS`.
    assert_eq!(
        fold_select_expr(AggregationFold::Latest),
        "(ARRAY_AGG(r.value ORDER BY r.window_end DESC, r.acceptance_sequence DESC))[1]::numeric"
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
        withdrawal_exclusion_clause()
            .contains("NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id)"),
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
         AND NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id)"
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
    assert!(aggregate_limit_clause(0).is_empty());
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
    assert_eq!(expr, "r.metadata ->> $1");
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
    assert_eq!(dimension_select_expr(&dim, &mut ctx), "r.metadata ->> $3");
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
        assert_eq!(dimension_select_expr(&dim, &mut ctx), expected, "{dim:?}");
        assert!(ctx.binds.is_empty(), "{dim:?} must bind nothing");
    }
}

// ── alias coupling ───────────────────────────────────────────────────────────

/// The ledger table's full column set, in declaration order.
/// [`ledger_columns_are_the_migrations_columns`] holds it to
/// `migrations/0001_init.sql`, so it is the schema's columns and not the subset
/// today's fragments happen to name. That difference is the point of the guard:
/// an arm added later reaches for a column no fragment mentions yet, and
/// `origin` is the one `DIVERGENCES.md` entry 15 proposes adding next. Every
/// occurrence of any of these must carry a table qualifier, or the fragment
/// binds to whatever the caller's `FROM` happens to expose.
const LEDGER_COLUMNS: &[&str] = &[
    "id",
    "tenant_id",
    "gts_type_id",
    "value",
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
    "acceptance_sequence",
    "metadata",
    "ingested_at",
];

/// The ledger table's column names as declared, in declaration order — the
/// name half of [`migration_probe::ledger_columns`], the crate's one parse of
/// `migrations/0001_init.sql`. The parser lives there rather than here because
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
        "LEDGER_COLUMNS must be migrations/0001_init.sql's usage_records columns, in order"
    );
}

/// The aliases `sql` may qualify a column with. `r`, the alias
/// [`ledger_from_clause`] declares, is always admissible. `w` is admissible
/// only in a fragment that declares it, so a fragment borrowing the withdrawal
/// subquery's alias without opening the subquery is an offender rather than a
/// pass.
fn aliases_for(sql: &str) -> &'static [u8] {
    if sql.contains("FROM usage_records w") {
        b"rw"
    } else {
        b"r"
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
        &MeterTypeId::new("gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~")
            .expect("valid meter id"),
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
    for fold in [
        AggregationFold::Sum,
        AggregationFold::Count,
        AggregationFold::Min,
        AggregationFold::Max,
        AggregationFold::Latest,
    ] {
        match fold {
            AggregationFold::Sum
            | AggregationFold::Count
            | AggregationFold::Min
            | AggregationFold::Max
            | AggregationFold::Latest => {}
        }
        fragments.push(fold_select_expr(fold).to_owned());
    }
    for dim in [
        AggregationDimension::TenantId,
        AggregationDimension::ResourceId,
        AggregationDimension::ResourceType,
        AggregationDimension::SubjectId,
        AggregationDimension::SubjectType,
        AggregationDimension::Metadata(MetadataKey::new("region").unwrap()),
    ] {
        match dim {
            AggregationDimension::TenantId
            | AggregationDimension::ResourceId
            | AggregationDimension::ResourceType
            | AggregationDimension::SubjectId
            | AggregationDimension::SubjectType
            | AggregationDimension::Metadata(_) => {}
        }
        let expr = dimension_select_expr(&dim, &mut ctx);
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
