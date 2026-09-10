//! Unit tests for the aggregation SQL builders. Pure (no DB): they pin the
//! exact SQL each [`AggregationFold`] emits, so a cast regression is caught
//! without Docker.

use usage_collector_sdk::{
    AggregationDimension, AggregationFold, MAX_AGGREGATION_BUCKETS, MetadataKey,
};

use super::super::bind::SqlBind;
use super::super::translate::SqlCtx;
use super::{
    agg_select_expr, aggregate_limit_clause, dimension_select_expr, latest_select_expr,
    withdrawal_exclusion_clause,
};

#[test]
fn every_aggregate_function_fold_casts_to_numeric() {
    assert_eq!(
        agg_select_expr(AggregationFold::Sum),
        Some("SUM(r.value)::numeric")
    );
    assert_eq!(
        agg_select_expr(AggregationFold::Count),
        Some("COUNT(*)::numeric")
    );
    assert_eq!(
        agg_select_expr(AggregationFold::Min),
        Some("MIN(r.value)::numeric")
    );
    assert_eq!(
        agg_select_expr(AggregationFold::Max),
        Some("MAX(r.value)::numeric")
    );
}

#[test]
fn latest_is_not_an_aggregate_function() {
    // `LATEST` is an ordered pick, not an aggregate function, so it has no
    // arm here and the caller must reach for `latest_select_expr` instead.
    // `None` means "rendered elsewhere", never "unsupported fold".
    assert_eq!(agg_select_expr(AggregationFold::Latest), None);
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
        latest_select_expr(),
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
    // dimension expr composes correctly after leading binds (e.g. `gts_id` at
    // `$1`).
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

/// Column names this module's fragments may reference. Every occurrence must
/// carry a table qualifier, or the fragment binds to whatever the caller's
/// `FROM` happens to expose.
const LEDGER_COLUMNS: &[&str] = &[
    "id",
    "value",
    "window_end",
    "acceptance_sequence",
    "tenant_id",
    "resource_id",
    "resource_type",
    "subject_id",
    "subject_type",
    "metadata",
    "invalidates",
];

/// The two aliases a fragment may qualify a column with: `r`, the outer
/// query's alias for `usage_records`, and `w`, the withdrawal subquery's own.
const ALIASES: &[u8] = b"rw";

/// Whether the ledger column starting at `at` carries one of [`ALIASES`] as
/// its qualifier. A bare column, or one qualified with any other alias, is
/// false.
fn is_qualified(sql: &str, at: usize) -> bool {
    let b = sql.as_bytes();
    at >= 2
        && b[at - 1] == b'.'
        && ALIASES.contains(&b[at - 2])
        && (at == 2 || !(b[at - 3].is_ascii_alphanumeric() || b[at - 3] == b'_'))
}

/// Every ledger column in `sql` that is not qualified with `r.` or `w.`.
/// Splits on identifier boundaries so `subject_id` is one token rather than a
/// `subject_type` plus an `id`.
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

/// Every SQL fragment this module emits, with a metadata dimension standing in
/// for the one caller-derived shape.
fn all_fragments() -> Vec<String> {
    let mut ctx = SqlCtx::new(1);
    let mut fragments: Vec<String> = vec![
        withdrawal_exclusion_clause().to_owned(),
        latest_select_expr().to_owned(),
    ];
    for fold in [
        AggregationFold::Sum,
        AggregationFold::Count,
        AggregationFold::Min,
        AggregationFold::Max,
        AggregationFold::Latest,
    ] {
        fragments.extend(agg_select_expr(fold).map(str::to_owned));
    }
    for dim in [
        AggregationDimension::TenantId,
        AggregationDimension::ResourceId,
        AggregationDimension::ResourceType,
        AggregationDimension::SubjectId,
        AggregationDimension::SubjectType,
        AggregationDimension::Metadata(MetadataKey::new("region").unwrap()),
    ] {
        fragments.push(dimension_select_expr(&dim, &mut ctx));
    }
    fragments
}

#[test]
fn every_fragment_qualifies_its_columns_with_the_r_alias() {
    // Nothing in the crate enforces that the aggregate caller aliases
    // `usage_records` as `r`; these fragments hard-code it, and a caller that
    // aliases differently produces invalid SQL at runtime with no compile
    // error. This is the half of that coupling this file can pin: no fragment
    // may name a bare column, and none may reach for an alias the others do
    // not share, so a fragment drifting off `r` is caught here rather than by
    // a query failing against a live database. It holds over fragments an arm
    // added later emits too, which the exact-string tests above cannot.
    for fragment in all_fragments() {
        assert!(
            misqualified_columns(&fragment).is_empty(),
            "{fragment} names {:?} without the outer query's `r` alias",
            misqualified_columns(&fragment)
        );
    }
}
