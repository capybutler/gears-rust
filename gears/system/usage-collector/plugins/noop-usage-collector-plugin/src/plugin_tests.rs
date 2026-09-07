//! Tests for the no-op storage backend's [`UsageCollectorPluginV1`] surface.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use usage_collector_sdk::{
    IdempotencyKey, MeterTypeId, ResourceRef, UsageCollectorPluginError, UsageCollectorPluginV1,
    UsageRecord, UsageRecordStatus,
};
use uuid::Uuid;

use super::NoopBackend;

fn sample_record(id: &str, idempotency_key: &str) -> UsageRecord {
    UsageRecord {
        id: Uuid::parse_str(id).expect("valid record id fixture"),
        gts_type_id: MeterTypeId::new("gts.cf.core.uc.usage_record.v1~test.uc.batch.order.v1~")
            .expect("meter type id fixture"),
        tenant_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("valid tenant uuid fixture"),
        resource_ref: ResourceRef::new("vm-1", "compute.vm").expect("valid resource ref fixture"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        value: Decimal::from(1),
        idempotency_key: IdempotencyKey::new(idempotency_key)
            .expect("valid idempotency key fixture"),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        window_start: time::OffsetDateTime::UNIX_EPOCH,
        window_end: time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    }
}

#[tokio::test]
async fn create_usage_records_preserves_input_order() {
    let backend = NoopBackend::new();
    let inputs = vec![
        sample_record("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "k-1"),
        sample_record("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "k-2"),
        sample_record("cccccccc-cccc-cccc-cccc-cccccccccccc", "k-3"),
    ];

    let outputs = backend
        .create_usage_records(inputs.clone())
        .await
        .expect("noop create_usage_records must succeed on echo");

    assert_eq!(
        outputs.len(),
        inputs.len(),
        "result vec length MUST match input vec length",
    );
    for (i, (input, output)) in inputs.iter().zip(outputs.into_iter()).enumerate() {
        let echoed = output.unwrap_or_else(|e| {
            panic!("noop must Ok-echo every input record, got error at index {i}: {e:?}")
        });
        assert_eq!(
            &echoed, input,
            "echoed record at index {i} MUST equal the input at the same index",
        );
    }
}

#[tokio::test]
async fn create_usage_records_rejects_empty_batch_as_internal() {
    let backend = NoopBackend::new();

    let err = backend
        .create_usage_records(Vec::new())
        .await
        .expect_err("an empty batch is a host-contract breach, not a success");

    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "empty batch MUST surface as Internal (non-retryable host-contract breach), got {err:?}",
    );
}

#[tokio::test]
async fn deactivate_usage_record_returns_not_found_with_target_id() {
    let backend = NoopBackend::new();
    let id = uuid::Uuid::from_u128(0x1234_5678_9ABC_DEF0);

    let err = backend
        .deactivate_usage_record(id)
        .await
        .expect_err("noop backend MUST surface UsageRecordNotFound");

    match err {
        UsageCollectorPluginError::UsageRecordNotFound { id: returned } => {
            assert_eq!(
                returned, id,
                "the not-found variant MUST echo the supplied target id",
            );
        }
        other => panic!("expected UsageRecordNotFound, got {other:?}"),
    }
}
