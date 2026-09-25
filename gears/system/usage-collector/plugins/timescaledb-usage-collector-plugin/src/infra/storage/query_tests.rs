// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use usage_collector_sdk::{MetadataFilter, MeterTypeId, TimeRange};

use super::bind::SqlBind;
use super::translate::SqlCtx;
use super::{
    MAX_PAGE_SIZE, effective_page_size, ledger_from_clause, push_metadata_filter_clauses,
    push_meter_and_range_clauses,
};

const DEFAULT: u64 = 100;

const VCPU_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";
const RANGE_FROM_UNIX: i64 = 1_699_920_000;
const RANGE_TO_UNIX: i64 = 1_700_006_400;

fn meter() -> MeterTypeId {
    MeterTypeId::new(VCPU_METER).expect("valid meter id")
}

fn range() -> TimeRange {
    TimeRange::new(
        time::OffsetDateTime::from_unix_timestamp(RANGE_FROM_UNIX).expect("valid ts"),
        time::OffsetDateTime::from_unix_timestamp(RANGE_TO_UNIX).expect("valid ts"),
    )
    .expect("a strictly ordered range")
}

#[test]
fn effective_page_size_defaults_when_top_omitted() {
    // No `$top` -> the store's default page size, unchanged.
    assert_eq!(effective_page_size(None, DEFAULT), DEFAULT);
}

#[test]
fn effective_page_size_passes_a_within_cap_top_through_unchanged() {
    // A caller `$top` below the cap is honored verbatim.
    assert_eq!(effective_page_size(Some(250), DEFAULT), 250);
}

#[test]
fn effective_page_size_allows_exactly_the_cap() {
    // The cap itself is a legal page size (inclusive bound).
    assert_eq!(
        effective_page_size(Some(MAX_PAGE_SIZE), DEFAULT),
        MAX_PAGE_SIZE
    );
}

#[test]
fn effective_page_size_floors_a_zero_top_to_one() {
    // A resolved page size of 0 would drive `LIMIT 0+1 = 1`, fetch the
    // look-ahead row, then `truncate(0)` — losing the page tail so
    // `rows.last()` is `None` and the list path 500s with "non-empty page lost
    // its tail". The REST surface cannot deliver a 0 (the toolkit `OData`
    // extractor rejects a zero page size — `$top=0` or `limit=0` — with
    // `InvalidLimit`), but an in-process SDK caller builds its own
    // `ODataQuery` and reaches neither that check nor the core gateway's
    // `prepare_list_query`. Floor to 1 (the smallest legal page) so both
    // list paths stay sound regardless.
    assert_eq!(effective_page_size(Some(0), DEFAULT), 1);
}

#[test]
fn effective_page_size_floors_a_zero_default_to_one() {
    // Belt-and-suspenders: even a (mis)configured zero default page size can
    // never resolve to a 0 `LIMIT`.
    assert_eq!(effective_page_size(None, 0), 1);
}

#[test]
fn effective_page_size_clamps_a_top_above_the_cap() {
    // The defense-in-depth backstop: an oversized `$top` that slipped past
    // the core gateway is clamped to the cap, never fed verbatim into
    // `LIMIT n+1 ... fetch_all` (an unbounded full-result-set read).
    assert_eq!(effective_page_size(Some(1_000_000), DEFAULT), MAX_PAGE_SIZE);
    assert_eq!(effective_page_size(Some(u64::MAX), DEFAULT), MAX_PAGE_SIZE);
}

// --- The fragments both ledger read paths share ---

#[test]
fn the_from_clause_declares_the_alias_every_fragment_qualifies_with() {
    // One constant, read by both read paths. The alias-qualification guard in
    // `aggregate_tests.rs` is the other half: it walks every fragment an
    // assembled statement is built from and refuses a bare column.
    assert_eq!(ledger_from_clause(), "usage_records r");
}

#[test]
fn the_range_predicate_reads_the_period_end_alone() {
    // `from <= window_end < to`
    // (`cpt-cf-usage-collector-adr-window-end-selection`), hoisted here so both
    // read paths inherit one spelling. Getting it wrong fails two contract
    // checks at once — `window-end-selection`, and `quantity-round-trip`, whose
    // read-back range is one second wide at the period end while the entry's
    // period began an hour earlier.
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_and_range_clauses(&meter(), range(), &mut ctx, &mut clauses);

    // Transcribed by hand.
    assert_eq!(
        clauses,
        vec![
            "r.gts_type_id = $1".to_owned(),
            "r.type_key = (SELECT k.type_key FROM usage_type_key k WHERE k.gts_type_id = $1)"
                .to_owned(),
            "r.window_end >= $2".to_owned(),
            "r.window_end < $3".to_owned(),
        ]
    );
    assert_eq!(
        ctx.binds.len(),
        3,
        "the key subquery reuses the meter's bind rather than binding it twice"
    );
    assert!(
        !clauses.iter().any(|c| c.contains("window_start")),
        "the predicate must not read the period start: overlap selects one \
         entry into two adjacent ranges and containment drops it out of both"
    );
}

#[test]
fn the_range_predicate_binds_the_meter_then_both_bounds_in_order() {
    // The clause text above is not enough on its own: swap the two bounds and
    // the range inverts to `window_end >= to AND window_end < from`, which
    // selects nothing, ever, without changing a character of the SQL.
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_and_range_clauses(&meter(), range(), &mut ctx, &mut clauses);

    assert_eq!(ctx.binds.len(), 3, "the meter and the two bounds");
    assert!(
        matches!(&ctx.binds[0], SqlBind::Str(s) if s == VCPU_METER),
        "$1 is the meter. got: {:?}",
        ctx.binds[0]
    );
    assert!(
        matches!(&ctx.binds[1], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_FROM_UNIX),
        "$2 is the inclusive lower bound. got: {:?}",
        ctx.binds[1]
    );
    assert!(
        matches!(&ctx.binds[2], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_TO_UNIX),
        "$3 is the exclusive upper bound. got: {:?}",
        ctx.binds[2]
    );
}

#[test]
fn the_metadata_side_channel_ands_across_filters_and_ors_within_one() {
    // AND across entries, OR within one entry's values, with the key and every
    // value bound. The column is alias-qualified like every other fragment.
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_metadata_filter_clauses(
        &[
            MetadataFilter::new("region", ["eu-west-1", "eu-west-2"]).expect("valid filter"),
            MetadataFilter::new("tier", ["gold"]).expect("valid filter"),
        ],
        &mut ctx,
        &mut clauses,
    );

    // Transcribed by hand.
    assert_eq!(
        clauses,
        vec![
            "r.metadata ->> $1 IN ($2, $3)".to_owned(),
            "r.metadata ->> $4 IN ($5)".to_owned(),
        ]
    );
    assert_eq!(ctx.binds.len(), 5, "two keys and three values");
}

#[test]
fn no_metadata_filters_leaves_the_where_clause_untouched() {
    // The side channel is optional; an absent one must add neither a clause nor
    // a bind, or every placeholder after it renumbers.
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_metadata_filter_clauses(&[], &mut ctx, &mut clauses);

    assert!(clauses.is_empty() && ctx.binds.is_empty());
}

#[test]
fn the_sdk_refuses_the_empty_value_set_the_false_arm_guards_against() {
    // The `FALSE` arm is defence-in-depth and **not reachable through this
    // constructor**, which is the whole reason it can only be documented rather
    // than exercised: pinning the refusal here is what says so, instead of a
    // test that claims to cover an arm it cannot reach. If the arm ever were
    // reachable, dropping the clause would widen the read rather than narrow
    // it, which is why it emits `FALSE` and not nothing.
    let empty: [&str; 0] = [];
    assert!(
        MetadataFilter::new("region", empty).is_err(),
        "the SDK refuses an empty value set at construction"
    );
}
