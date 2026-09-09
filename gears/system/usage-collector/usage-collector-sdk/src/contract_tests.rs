//! Tests for the contract suite itself.
//!
//! Three different things are asserted here, and none is a plugin's
//! conformance. The first is that the suite runs and passes against a
//! backend built to conform, which is what makes a violation reported
//! against a real plugin worth reading. The second is that the three
//! coverage constants still partition DESIGN's seven checks, so a passing
//! run cannot read as a complete one. The third is the reference backend's
//! own fail-closed posture, which is not a contract check but is the thing
//! a plugin author copies.
//!
//! What this file cannot establish is that the checks *discriminate*: the
//! reference backend and the assertions were written alongside each other,
//! so a check too weak to catch a non-conforming backend passes here
//! exactly as a good one does. Closing that seam takes a deliberately
//! non-conforming subject, and it is a separate piece of work.

use std::collections::BTreeSet;

use rust_decimal::Decimal;
use toolkit_odata::{ODataQuery, ast};
use uuid::Uuid;

use super::{
    BLOCKED_CHECKS, HARNESS_FAULT, IMPLEMENTED_CHECKS, UNWRITTEN_CHECKS,
    reference::InMemoryReferencePlugin, run_all,
};
use crate::error::UsageCollectorPluginError;
use crate::models::{
    CreateUsageRecord, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef, UsageRecord,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::time_range::TimeRange;

#[tokio::test]
async fn the_reference_backend_conforms() {
    let plugin = InMemoryReferencePlugin::new();

    let violations = run_all(&plugin).await;

    assert!(
        violations.is_empty(),
        "the reference backend is the suite's own subject and must pass every implemented \
         check; it reported: {violations:#?}"
    );
}

/// The blocked list names checks DESIGN §3.3 actually declares, and does not
/// name one this suite implements.
///
/// Without this, `BLOCKED_CHECKS` is prose: a typo, or a row left behind
/// after a check became writable, reads exactly like an honest gap. The
/// name says *cannot express* rather than *not implemented* because four of
/// the seven are unimplemented and only two of those are blocked — the
/// other two are in `UNWRITTEN_CHECKS`.
#[test]
fn the_blocked_checks_are_the_ones_the_spi_cannot_express() {
    let blocked: BTreeSet<&str> = BLOCKED_CHECKS.iter().map(|(check, _)| *check).collect();

    assert_eq!(
        blocked,
        BTreeSet::from(["feed-snapshot-and-replay", "latest-tie-break"]),
        "the blocked set must be exactly the two DESIGN section 3.3 checks the current SPI cannot \
         express; a name that is not in DESIGN's table, or one whose check has since become \
         writable, reads as an honest gap and is not one"
    );
    for check in IMPLEMENTED_CHECKS {
        assert!(
            !blocked.contains(check),
            "`{check}` is implemented and run by `run_all`, so listing it as blocked would \
             under-report the suite's coverage"
        );
    }
    for (check, reason) in BLOCKED_CHECKS {
        assert!(
            !reason.trim().is_empty(),
            "`{check}` is listed as blocked with no reason: the entry exists to say what \
             unblocks it, and an empty one only hides the check"
        );
    }
}

/// Every check DESIGN §3.3 declares is implemented, blocked, or named as
/// unwritten — exactly once.
///
/// This is what makes the module's coverage claim structural instead of
/// narrative. `run_all` returning no violations says nothing about a check
/// it never ran, and "run this suite" is the acceptance criterion for
/// porting a storage backend, so a suite that runs three checks must not
/// read as a suite that ran seven. Asserting the partition means a check cannot
/// half-land — implemented but still listed unwritten, or written and
/// listed nowhere — without this failing, and `UNWRITTEN_CHECKS` empties
/// itself as the work lands rather than needing someone to remember.
#[test]
fn the_three_coverage_constants_partition_the_design_checks() {
    /// The seven names in DESIGN §3.3's "Plugin contract tests" table.
    const DESIGN_CHECKS: [&str; 7] = [
        "window-end-selection",
        "invalidation-excluded-from-fold",
        "at-most-one-invalidation",
        "dedup-identity-over-window",
        "quantity-round-trip",
        "feed-snapshot-and-replay",
        "latest-tie-break",
    ];

    let mut claimed: Vec<&str> = IMPLEMENTED_CHECKS.to_vec();
    claimed.extend_from_slice(UNWRITTEN_CHECKS);
    claimed.extend(BLOCKED_CHECKS.iter().map(|(check, _)| *check));

    let unique: BTreeSet<&str> = claimed.iter().copied().collect();
    assert_eq!(
        unique.len(),
        claimed.len(),
        "a check is named in more than one coverage constant, so the three do not partition \
         anything: {claimed:?}"
    );
    assert_eq!(
        unique,
        BTreeSet::from(DESIGN_CHECKS),
        "the implemented, unwritten and blocked constants must together be exactly DESIGN \
         section 3.3's seven checks: no invented name, and nothing left unaccounted for"
    );
    assert!(
        !unique.contains(HARNESS_FAULT),
        "`{HARNESS_FAULT}` marks a fault in the suite rather than a DESIGN check, so it must \
         never appear in the coverage constants"
    );
}

/// An uninterpretable comparison never admits a row, under `eq` or `ne`.
///
/// This is about the reference backend rather than about the contract, so
/// it is a unit test here and not a check in [`run_all`]: a plugin's own
/// scope projection is its business, and the suite has no `scope-gating`
/// check yet to fold it into.
///
/// The posture is load-bearing precisely *because* this backend is an
/// exemplar. Its module docs say a real plugin projects the same
/// obligations into SQL, so a hole here is a hole a plugin author inherits
/// — and the widening one is a cross-tenant read, not a cosmetic
/// over-match. The gear itself denies the very pairing exercised below: a
/// UUID-typed scope field against a value that is not a UUID lifts to
/// `AuthorizationDenied` in `authz::scope_value_to_ast`
/// (`usage-collector/src/domain/authz_tests.rs`). An exemplar more
/// permissive than the thing it models is the worst direction for the
/// error to run in.
///
/// `ne` is where this bites. `eq` folds "no match" and "cannot interpret"
/// into one exclusion harmlessly; `ne` inverts the answer, so a single
/// `bool` makes an unanswerable comparison *admit*. The scope
/// `Or(tenant_id eq <other tenant>, tenant_id ne "not-a-uuid")` names only
/// a tenant that does not own the row and still admitted it before
/// `value_matches` reported interpretability as an outcome of its own.
#[tokio::test]
async fn an_uninterpretable_comparison_admits_no_row_under_either_operator() {
    let plugin = InMemoryReferencePlugin::new();
    let record = scope_probe_record();
    let id = record.id;
    let other_tenant = Uuid::from_u128(0xdead_beef_0000_4000_8000_0000_0000_0002);
    plugin
        .create_usage_record(record)
        .await
        .expect("the reference backend admits a well-formed entry");

    // The reviewer's case, by name: a scope naming only another tenant,
    // widened back open by an `ne` against an operand no comparison can
    // read. This is a cross-tenant read in its most legible form.
    let cross_tenant = ast::Expr::Or(
        Box::new(compare(
            "tenant_id",
            ast::CompareOperator::Eq,
            ast::Value::Uuid(other_tenant),
        )),
        Box::new(compare(
            "tenant_id",
            ast::CompareOperator::Ne,
            ast::Value::String("not-a-uuid".to_owned()),
        )),
    );
    assert_not_found(
        &plugin,
        id,
        &cross_tenant,
        "a scope whose only satisfiable disjunct is an uninterpretable `ne` names no tenant that \
         owns this row, so admitting it is a cross-tenant read",
    )
    .await;

    // The same hole with no disjunction to hide behind: one node, no
    // tenant named at all, admitting every row in the ledger.
    assert_not_found(
        &plugin,
        id,
        &compare(
            "tenant_id",
            ast::CompareOperator::Ne,
            ast::Value::Bool(true),
        ),
        "`tenant_id ne <bool>` is a comparison this backend cannot make, so it must exclude \
         rather than match every row",
    )
    .await;

    // `eq` was already right, and stays right.
    assert_not_found(
        &plugin,
        id,
        &compare(
            "tenant_id",
            ast::CompareOperator::Eq,
            ast::Value::Bool(true),
        ),
        "an uninterpretable `eq` excludes the row",
    )
    .await;

    // The case the fix must not break: an absent attribute is unanswerable
    // through `record_field`, and `ne` over it stays `false`. The fixture
    // carries no `subject_ref`.
    assert_not_found(
        &plugin,
        id,
        &compare(
            "subject_id",
            ast::CompareOperator::Ne,
            ast::Value::String("someone".to_owned()),
        ),
        "`ne` against an attribute the row does not carry is SQL's NULL comparison: not selected",
    )
    .await;

    // The positive. An interpretable `ne` that genuinely does not match
    // still admits — without this, a fix that simply refused `ne` outright
    // would pass every assertion above while refusing valid scopes.
    let interpretable = ast::Expr::And(
        Box::new(compare(
            "tenant_id",
            ast::CompareOperator::Eq,
            ast::Value::Uuid(super::CONTRACT_TENANT_ID),
        )),
        Box::new(compare(
            "resource_type",
            ast::CompareOperator::Ne,
            ast::Value::String("some.other.type".to_owned()),
        )),
    );
    let admitted = plugin
        .get_usage_record(id, &interpretable)
        .await
        .expect("an interpretable `ne` the row does not match must still admit it");
    assert_eq!(
        admitted.id, id,
        "the row admitted under an interpretable scope must be the row asked for"
    );
}

/// A `<identifier> <op> <literal>` node.
fn compare(field: &str, op: ast::CompareOperator, value: ast::Value) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier(field.to_owned())),
        op,
        Box::new(ast::Expr::Value(value)),
    )
}

/// Asserts a scope does not admit the row, which the SPI spells as
/// `UsageRecordNotFound` — never a distinguishable "denied".
async fn assert_not_found(
    plugin: &InMemoryReferencePlugin,
    id: Uuid,
    scope: &ast::Expr,
    why: &str,
) {
    let outcome = plugin.get_usage_record(id, scope).await;
    assert!(
        matches!(
            outcome,
            Err(UsageCollectorPluginError::UsageRecordNotFound { id: reported }) if reported == id
        ),
        "{why}"
    );
}

/// The subject-less row every scope above is evaluated against.
fn scope_probe_record() -> UsageRecord {
    CreateUsageRecord {
        gts_type_id: MeterTypeId::new(super::CONTRACT_METER_TYPE_ID).expect("valid meter type id"),
        tenant_id: super::CONTRACT_TENANT_ID,
        resource_ref: ResourceRef::new(super::CONTRACT_RESOURCE_ID, super::CONTRACT_RESOURCE_TYPE)
            .expect("valid resource reference"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        value: Decimal::ONE,
        idempotency_key: IdempotencyKey::new("scope-probe").expect("valid idempotency key"),
        invalidation: None,
        window_start: super::FIXTURE_EPOCH,
        window_end: super::FIXTURE_EPOCH,
    }
    .try_into_usage_record(RecordOrigin::Live)
    .expect("the probe fixture is projectable")
}

/// The same untranslatable node excludes as a scope and refuses as a filter.
///
/// These are opposite outcomes from one input shape, and that is the point.
/// The module's prose has always said *"an untranslatable predicate is a
/// refused query, never a dropped conjunct"* while the code answered an
/// empty selection for both, which is a silently wrong answer to a question
/// the caller did not ask. It is the `ne` defect in a second place: two
/// outcomes folded into one `false`.
///
/// The path is live rather than hypothetical. `toolkit-odata` parses
/// `contains`, and the gear's `reject_reserved_filter_fields` recurses
/// *through* `Expr::Function` rather than refusing it, then ANDs the
/// caller's raw expression onto the compiled scope before dispatch — so
/// `$filter=contains(resource_id,'x')` reaches `list_usage_records` today.
///
/// The scope half must stay an exclusion: refusing a lookup because a grant
/// could not be read would turn a fail-closed denial into a 500, and worse,
/// any weakening there is a cross-tenant read.
#[tokio::test]
async fn an_untranslatable_node_refuses_a_caller_filter_and_excludes_a_scope() {
    let plugin = InMemoryReferencePlugin::new();
    let record = scope_probe_record();
    let id = record.id;
    let meter = record.gts_type_id.clone();
    let window_end = record.window_end;
    plugin
        .create_usage_record(record)
        .await
        .expect("the reference backend admits a well-formed entry");

    // `contains(resource_id, 'contract')` — a function node, which this
    // backend cannot translate. It would match the row if it could.
    let function = ast::Expr::Function(
        "contains".to_owned(),
        vec![
            ast::Expr::Identifier("resource_id".to_owned()),
            ast::Expr::Value(ast::Value::String("contract".to_owned())),
        ],
    );

    // As a caller filter: refused, not answered with an empty page.
    let range = TimeRange::new(
        window_end,
        window_end.saturating_add(time::Duration::seconds(1)),
    )
    .expect("an ordered probe range");
    let query = ODataQuery::new().with_filter(function.clone());
    let refused = plugin
        .list_usage_records(meter, range, &query, &[])
        .await
        .expect_err("an untranslatable caller filter must refuse the read, not return no rows");
    assert!(
        matches!(refused, UsageCollectorPluginError::Internal(ref detail) if detail.contains("contains")),
        "the refusal must name the node kind it could not translate, so an operator can see \
         which predicate was rejected; got: {refused}"
    );

    // The very same node as a scope: excluded, and reported exactly as an
    // absent row so the surface stays free of an existence oracle.
    let excluded = plugin.get_usage_record(id, &function).await;
    assert!(
        matches!(
            excluded,
            Err(UsageCollectorPluginError::UsageRecordNotFound { id: reported }) if reported == id
        ),
        "a scope this backend cannot read must grant nothing and say so as `UsageRecordNotFound`, \
         never as a failure the caller could tell apart from an absent row; got: {excluded:?}"
    );

    // And the translatable filter still reads: the refusal is about the
    // node, not about filters in general.
    let translatable = ODataQuery::new().with_filter(compare(
        "resource_id",
        ast::CompareOperator::Eq,
        ast::Value::String(super::CONTRACT_RESOURCE_ID.to_owned()),
    ));
    let page = plugin
        .list_usage_records(
            MeterTypeId::new(super::CONTRACT_METER_TYPE_ID).expect("valid meter type id"),
            range,
            &translatable,
            &[],
        )
        .await
        .expect("a translatable filter must still be served");
    assert_eq!(
        page.items.len(),
        1,
        "the probe row matches the translatable filter, so refusing it would mean the fix had \
         over-reached from the untranslatable node to every filter"
    );
}
