//! Unit tests for the reconciliation types.

use bigdecimal::BigDecimal;
use uuid::Uuid;

use super::{ReconciliationMetadata, ReconciliationScope};
use crate::models::MeterTypeId;

fn meter() -> MeterTypeId {
    // Two `~`-terminated segments, each `vendor.package.namespace.type.vMAJOR`
    // — the `gts-id` grammar needs at least five dot-separated tokens per
    // segment. See `feed_tests.rs`'s `meter()` for the working precedent and
    // `contract/fixtures.rs`'s `CONTRACT_METER_TYPE_ID` for the canonical form.
    MeterTypeId::new("gts.cf.core.uc.usage_record.v1~test.reconciliation._.stored_volume.v1~")
        .expect("the fixture meter id is well formed")
}

#[test]
fn a_scope_holding_no_entries_reports_zero_and_absent_watermarks() {
    let empty = ReconciliationMetadata::empty();

    assert_eq!(empty.accepted_count, 0);
    assert!(
        empty.max_accepted_at.is_none() && empty.max_window_end.is_none(),
        "DESIGN section 3.1 makes both watermarks absent when the scope holds no entries"
    );
    assert!(
        empty.quantity_summary.is_none(),
        "a scope with no entries has no fold to report; a fold defined over an empty selection \
         is the plugin's to supply, not this constructor's to guess"
    );
}

#[test]
fn a_sum_scope_can_report_a_defined_zero_distinctly_from_an_absent_fold() {
    let summed = ReconciliationMetadata {
        accepted_count: 2,
        quantity_summary: Some(BigDecimal::from(0)),
        ..ReconciliationMetadata::empty()
    };

    assert_eq!(
        summed.quantity_summary,
        Some(BigDecimal::from(0)),
        "SUM over a range holding nothing but a withdrawn pair is a defined 0, which must not \
         read as the absent fold MAX reports over the same range"
    );
    assert_eq!(
        summed.accepted_count, 2,
        "accepted_count reports ingestion activity, so it counts both entries of a withdrawn \
         pair even where the fold excludes them"
    );

    let same_summed = ReconciliationMetadata {
        accepted_count: 2,
        quantity_summary: Some(BigDecimal::from(0)),
        ..ReconciliationMetadata::empty()
    };
    assert_eq!(
        summed, same_summed,
        "PartialEq compares the whole struct, not just the field this test names"
    );
}

#[test]
fn a_scope_names_one_tenant_and_one_meter() {
    let tenant_id = Uuid::from_u128(1);
    let scope = ReconciliationScope {
        tenant_id,
        gts_type_id: meter(),
    };

    assert_eq!(scope.tenant_id, tenant_id);
    assert_eq!(scope.gts_type_id, meter());

    let same_scope = ReconciliationScope {
        tenant_id,
        gts_type_id: meter(),
    };
    assert_eq!(
        scope, same_scope,
        "PartialEq compares the whole struct, not just the field this test names"
    );
}
