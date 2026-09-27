//! Tests for the contract suite itself.
//!
//! Four different things are asserted here, in the order the file puts
//! them, and none is a plugin's conformance.
//!
//! The first is that the suite runs and passes against a backend built to
//! conform, which is what makes a violation reported against a real plugin
//! worth reading.
//!
//! The second is that the checks *discriminate*. The reference backend and
//! the assertions were written alongside each other, so the first assertion
//! establishes that the suite **runs** and nothing about whether any check
//! would notice a non-conforming plugin — and a check that cannot fail is
//! worse than a missing one, because a port is accepted on it and it reads
//! as coverage. [`super::contract_mutants`] holds seven deliberately
//! non-conforming subjects, each behaviourally the reference backend wrong
//! in exactly one plausible way, and
//! [`each_check_fails_against_its_own_defect_and_no_other`] asserts a whole
//! column against each of them.
//!
//! The third is that the three coverage constants still partition DESIGN's
//! sixteen checks, so a passing run cannot read as a complete one.
//!
//! The fourth is the reference backend's own fail-closed posture, which is
//! not a contract check but is the thing a plugin author copies.

use std::collections::BTreeSet;

use toolkit_odata::{ODataQuery, ast};
use uuid::Uuid;

use super::contract_mutants::{Defect, mutant};
use super::{
    ADDITIONAL_CHECKS, AT_MOST_ONE_INVALIDATION, BLOCKED_CHECKS, DEDUP_IDENTITY_OVER_WINDOW,
    DedupLevel, HARNESS_FAULT, IMPLEMENTED_CHECKS, INVALIDATION_EXCLUDED_FROM_FOLD,
    QUANTITY_ROUND_TRIP, SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH, UNWRITTEN_CHECKS,
    WINDOW_END_SELECTION, reference::InMemoryReferencePlugin, run_all,
};
use crate::error::UsageCollectorPluginError;
use crate::feed::{FeedPage, FeedPosition, FeedStart};
use crate::models::{
    CreateUsageRecord, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef, UsageRecord,
};
use crate::plugin_api::UsageCollectorPluginV1;
use crate::quantity::UsageQuantity;
use crate::time_range::TimeRange;

#[tokio::test]
async fn the_reference_backend_conforms() {
    let plugin = InMemoryReferencePlugin::new();

    let violations = run_all(&plugin, DedupLevel::Linearizable).await;

    assert!(
        violations.is_empty(),
        "the reference backend is the suite's own subject and must pass every implemented \
         check; it reported: {violations:#?}"
    );
}

/// The same run under [`DedupLevel::Eventual`], which is the only declaration
/// that reaches `at-most-one-invalidation`'s post-convergence ledger read and
/// `COUNT` fold. A backend that decides every write as it commits satisfies
/// the weaker declaration too, with a zero bound.
#[tokio::test]
async fn the_reference_backend_conforms_under_an_eventual_declaration() {
    let plugin = InMemoryReferencePlugin::new();

    let violations = run_all(
        &plugin,
        DedupLevel::Eventual {
            convergence_bound: std::time::Duration::ZERO,
        },
    )
    .await;

    assert!(
        violations.is_empty(),
        "the reference backend must pass every implemented check under an `Eventual` \
         declaration; it reported: {violations:#?}"
    );
}

/// One row per defect: the subject, and the checks `run_all` must report
/// against it.
///
/// The check names are the exported constants rather than string literals.
/// A literal here would go on matching a constant that had been respelled,
/// and the row would then assert nothing about the check it names.
///
/// **Every row but one names a single check**, which is what makes the
/// matrix a statement about discrimination. The exception is
/// [`Defect::SelectsOnWindowStart`], and it is a real overlap between two
/// checks rather than a mutant wrong twice: `quantity-round-trip` reads its
/// entries back over a range around each entry's `window_end`, and its
/// fixtures start an hour earlier, so a backend selecting on `window_start`
/// returns none of them and the check reports that it could not compare a
/// quantity at all. The dependency is the suite's, not the subject's — the
/// quantity check cannot be answered by a backend that fails period-end
/// selection — so the row names both rather than the assertion being
/// loosened to admit one.
const DISCRIMINATION_MATRIX: &[(Defect, &[&str])] = &[
    (Defect::QuantityThroughFloat, &[QUANTITY_ROUND_TRIP]),
    (
        Defect::SelectsOnWindowStart,
        &[WINDOW_END_SELECTION, QUANTITY_ROUND_TRIP],
    ),
    (Defect::DedupIgnoresThePeriod, &[DEDUP_IDENTITY_OVER_WINDOW]),
    (
        Defect::FoldsTheInvalidation,
        &[INVALIDATION_EXCLUDED_FROM_FOLD],
    ),
    (
        Defect::AbsorbsAWithdrawalWithAnotherReason,
        &[AT_MOST_ONE_INVALIDATION],
    ),
    (
        Defect::RefusesAWithdrawalWithTheSameReason,
        &[AT_MOST_ONE_INVALIDATION],
    ),
    (
        Defect::IgnoresScopeOnThePointRead,
        &[SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH],
    ),
];

/// Every check fails against a backend that gets its rule wrong, and passes
/// against every other backend.
///
/// The second half is what makes this a test of *discrimination* rather than
/// of sensitivity. A check that fails against all seven mutants is not
/// detecting its own rule; it is detecting that something is different. So
/// each row asserts a full column: the named check fails, and the others
/// still pass against the same mutant.
///
/// The subjects are in [`super::contract_mutants`], which also says why each
/// is built by wrapping the reference backend or by carrying a ledger of its
/// own, and why none of it is a switch inside `reference.rs`.
///
/// The closing assertion is against the checks the coverage constants
/// *declare* — `IMPLEMENTED_CHECKS` together with `ADDITIONAL_CHECKS` — not
/// against `run_all`'s body. A check added to the suite with no subject to
/// fail it would otherwise sit in the matrix's blind spot, which is the very
/// thing this test exists to take away.
///
/// The distinction is worth stating because it bounds what this catches. A
/// check added to `run_all` **and** to a coverage constant, with no mutant,
/// is caught here. One added to `run_all` and to neither constant escapes
/// this test and the partition test alike — nothing ties `run_all`'s call
/// list to the constants, and that gap predates the matrix.
#[tokio::test]
async fn each_check_fails_against_its_own_defect_and_no_other() {
    let mut named: BTreeSet<&str> = BTreeSet::new();
    for (defect, expected) in DISCRIMINATION_MATRIX {
        let plugin = mutant(*defect);
        let failed: BTreeSet<&str> = run_all(plugin.as_ref(), DedupLevel::Linearizable)
            .await
            .into_iter()
            .map(|violation| violation.check)
            .collect();
        let expected: BTreeSet<&str> = expected.iter().copied().collect();
        named.extend(expected.iter().copied());

        assert_eq!(
            failed, expected,
            "the `{defect:?}` subject is behaviourally the reference backend wrong in exactly one \
             way, and `run_all` must report exactly the checks that rule belongs to. A check \
             missing from the reported set cannot catch the mistake it exists for; an extra one \
             is either a subject wrong in a second way or a check detecting difference rather \
             than its own rule, and both make the suite read as coverage it does not have."
        );
    }

    let declared_by_the_coverage_constants: BTreeSet<&str> = IMPLEMENTED_CHECKS
        .iter()
        .chain(ADDITIONAL_CHECKS)
        .copied()
        .collect();
    assert_eq!(
        named, declared_by_the_coverage_constants,
        "every check `IMPLEMENTED_CHECKS` and `ADDITIONAL_CHECKS` declare must be named by some \
         row of the discrimination matrix, and the matrix must name no check they do not declare. \
         A declared check with no subject built to fail it is a check nothing here establishes \
         anything about. This is a statement about the coverage constants, not about `run_all`'s \
         call list: a check added to `run_all` and to neither constant escapes this assertion, \
         and see this test's doc for why that gap is not this test's to close."
    );
}

/// Nothing is listed as beyond the SPI's reach that this suite implements,
/// that it also calls merely unwritten, or that carries a justification
/// already known to be false — and, today, nothing is listed at all.
///
/// `BLOCKED_CHECKS` says *cannot express* rather than *not implemented*,
/// and that is a claim about the shape of [`UsageCollectorPluginV1`] rather
/// than about this crate's progress. **No test can check it.** A trait's
/// method set is not reachable at runtime on stable Rust, and the claim is
/// not even always about a method: of the two entries this constant used to
/// hold, one was blocked on a missing SPI method (`read_feed_page`, which
/// the SPI now declares) and the other on a rule in DESIGN's prose that
/// read an `acceptance_sequence` field the record does not carry — a rule
/// §3.1 has since settled differently, so the check became writable with
/// no field ever added. A guard that reflected over the trait would have
/// caught the first and passed the second.
///
/// So this test does three things it can do and names the one it cannot.
///
/// The structural assertions come first, and each arms itself the moment an
/// entry is added: a blocked check must not be in `IMPLEMENTED_CHECKS` (a
/// check that landed and stayed listed under-reports coverage), must not
/// also be in `UNWRITTEN_CHECKS` (the two make incompatible claims about
/// the same name), must carry a reason, and must not carry one of
/// `RETIRED_JUSTIFICATIONS` — the two this constant was caught holding,
/// both provably false and both cheap to resurrect from an old commit.
///
/// The emptiness assertion comes last, deliberately, so an entry that is
/// structurally wrong is diagnosed as wrong rather than merely as new. It
/// is a stop sign, not a snapshot. The previous version of this test
/// asserted `BTreeSet::from(["feed-snapshot-and-replay",
/// "latest-tie-break"])` and its own doc claimed to catch "a row that
/// stayed here after its check became writable" — yet both rows went
/// stale underneath it and it passed, because it pinned the *names* while
/// the rot was in the *reasons*. Emptiness makes no claim that can rot
/// that way. Its whole job is to stop the next author, who must then
/// establish by hand, and record in the entry, that:
///
/// 1. no method on [`UsageCollectorPluginV1`] lets the check be written,
///    naming the method that would;
/// 2. the DESIGN rule the check asserts still needs the thing the SPI
///    lacks — the trap the `latest-tie-break` row fell into;
/// 3. `UNWRITTEN_CHECKS` is not the honest home for it instead.
///
/// A limitation named beats a guard that cannot fire, which is exactly what
/// the hardcoded set was.
#[test]
fn the_blocked_checks_are_the_ones_the_spi_cannot_express() {
    /// Justifications this constant has already been caught holding after
    /// they stopped being true. Matched as substrings of a reason, so a
    /// reworded revival is caught with the original.
    ///
    /// **Substring matching on prose over-rejects, and that is the accepted
    /// trade.** A future entry that mentions `acceptance_sequence` for some
    /// unrelated and entirely legitimate reason fails this assertion too.
    /// The cost is bounded to a reword because of where the guard sits: it
    /// can only fire after someone has deliberately pushed past the
    /// emptiness stop sign below, so a false positive lands on an author who
    /// is already editing this test and reading this doc, not on a passer-by
    /// — and the failure is loud rather than a silently corrupted claim. If
    /// you are that author: say the same thing without the retired phrase,
    /// or drop the phrase from this list with a note saying why it can no
    /// longer mislead.
    const RETIRED_JUSTIFICATIONS: [&str; 2] = ["no feed method", "acceptance_sequence"];

    let blocked: BTreeSet<&str> = BLOCKED_CHECKS.iter().map(|(check, _)| *check).collect();

    for check in IMPLEMENTED_CHECKS {
        assert!(
            !blocked.contains(check),
            "`{check}` is implemented and run by `run_all`, so listing it as blocked would \
             under-report the suite's coverage"
        );
    }
    for check in UNWRITTEN_CHECKS {
        assert!(
            !blocked.contains(check),
            "`{check}` is named as blocked and as unwritten, which are incompatible claims: one \
             says the SPI cannot express the check, the other that it can and nobody has written \
             it. A reader cannot tell which is meant, and the partition test cannot tell either"
        );
    }
    for (check, reason) in BLOCKED_CHECKS {
        assert!(
            !reason.trim().is_empty(),
            "`{check}` is listed as blocked with no reason: the entry exists to say what \
             unblocks it, and an empty one only hides the check"
        );
        for retired in RETIRED_JUSTIFICATIONS {
            assert!(
                !reason.contains(retired),
                "`{check}` is blocked on `{retired}`, which was true once and is not now. The \
                 SPI declares `read_feed_page`, and the DESIGN section 3.1 order reads \
                 `window_end`, `accepted_at` and `id` rather than an `acceptance_sequence` the \
                 record never carried. Both of these justifications were held here after they \
                 became false; neither is a blocker again without the SPI changing back"
            );
        }
    }

    assert!(
        blocked.is_empty(),
        "`BLOCKED_CHECKS` names {blocked:?}, and this assertion exists to stop you here. Nothing \
         automated can confirm that a check is beyond the SPI's reach: the trait's method set is \
         not visible at runtime, and the last two entries here went stale without a single test \
         failing. Before changing this assertion, establish by hand that no method on \
         `UsageCollectorPluginV1` lets the check be written, that the DESIGN rule it asserts \
         still needs what the SPI lacks, and that `UNWRITTEN_CHECKS` is not the honest home for \
         it. Then say which of those you checked, in the entry"
    );
}

/// Every check DESIGN §3.3 declares is implemented, blocked, or named as
/// unwritten — exactly once, and nothing else is any of the three.
///
/// This is what makes the module's coverage claim structural instead of
/// narrative. `run_all` returning no violations says nothing about a check
/// it never ran, and "run this suite" is the acceptance criterion for
/// porting a storage backend, so a suite that runs six checks must not
/// read as a suite that ran sixteen. Asserting the partition means a check
/// cannot half-land — implemented but still listed unwritten, or written
/// and listed nowhere — without this failing, and `UNWRITTEN_CHECKS`
/// empties itself as the work lands rather than needing someone to
/// remember.
///
/// `ADDITIONAL_CHECKS` is deliberately outside the partition and asserted
/// against it rather than folded into it. `run_all` runs a check DESIGN
/// does not tabulate, and admitting it to `IMPLEMENTED_CHECKS` would force
/// the equality below down to a subset check — which no longer catches a
/// DESIGN check written and listed nowhere, the exact failure the partition
/// exists for. Held disjoint instead, the fourth constant cannot become a
/// place to park a DESIGN name to escape the accounting.
#[test]
fn the_three_coverage_constants_partition_the_design_checks() {
    /// The sixteen names in DESIGN §3.3's "Plugin contract tests" table.
    const DESIGN_CHECKS: [&str; 16] = [
        "window-end-selection",
        "invalidation-excluded-from-fold",
        "at-most-one-invalidation",
        "record-and-invalidation-distinct-identity",
        "converged-target-lookup",
        "dedup-identity-over-window",
        "dedup-floor",
        "dedup-concurrent",
        "quantity-round-trip",
        "server-field-round-trip",
        "feed-snapshot-and-replay",
        "feed-completeness",
        "feed-bootstrap-position",
        "feed-retention-refusal",
        "feed-position-bounded",
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
         section 3.3's sixteen checks: no invented name, and nothing left unaccounted for"
    );
    assert!(
        !unique.contains(HARNESS_FAULT),
        "`{HARNESS_FAULT}` marks a fault in the suite rather than a DESIGN check, so it must \
         never appear in the coverage constants"
    );

    for check in ADDITIONAL_CHECKS {
        assert!(
            !unique.contains(check),
            "`{check}` is named in `ADDITIONAL_CHECKS`, which is for the checks DESIGN section \
             3.3 does not tabulate, and it also appears in the three constants that partition \
             DESIGN's sixteen. One of the two is wrong: either the name belongs in the partition \
             and not here, or the partition has grown a name DESIGN never wrote"
        );
    }
    assert!(
        ADDITIONAL_CHECKS.contains(&SCOPE_IS_A_FILTER_ON_EVERY_READ_PATH),
        "`run_all` runs the scope check, so leaving it out of `ADDITIONAL_CHECKS` would make a \
         caller reporting coverage under-report what the run actually covered"
    );
}

/// An uninterpretable comparison never admits a row, under `eq` or `ne`.
///
/// This is about the reference backend rather than about the contract, so
/// it is a unit test here and not a check in [`run_all`]: a plugin's own
/// scope projection is its business. The suite's own
/// `scope-is-a-filter-on-every-read-path` check asserts the obligation
/// every backend owes — a row outside the scope is withheld — and says
/// nothing about how a backend that cannot *read* part of a scope should
/// dispose of it, which is what this test pins for the exemplar.
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
            ast::Value::Uuid(super::fixtures::CONTRACT_TENANT_ID),
        )),
        Box::new(compare(
            "resource_type",
            ast::CompareOperator::Ne,
            ast::Value::String("some.other.type".to_owned()),
        )),
    );
    let admitted = plugin
        .get_usage_record(id, &interpretable, false)
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
    let outcome = plugin.get_usage_record(id, scope, false).await;
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
        gts_type_id: MeterTypeId::new(super::fixtures::CONTRACT_METER_TYPE_ID)
            .expect("valid meter type id"),
        tenant_id: super::fixtures::CONTRACT_TENANT_ID,
        resource_ref: ResourceRef::new(
            super::fixtures::CONTRACT_RESOURCE_ID,
            super::fixtures::CONTRACT_RESOURCE_TYPE,
        )
        .expect("valid resource reference"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        quantity: UsageQuantity::parse("1").expect("fixture quantity"),
        idempotency_key: Some(IdempotencyKey::new("scope-probe").expect("valid idempotency key")),
        invalidation: None,
        window_start: super::fixtures::FIXTURE_EPOCH,
        window_end: super::fixtures::FIXTURE_EPOCH,
    }
    .try_into_usage_record(RecordOrigin::Live, super::fixtures::CONTRACT_ACCEPTED_AT)
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
    let excluded = plugin.get_usage_record(id, &function, false).await;
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
        ast::Value::String(super::fixtures::CONTRACT_RESOURCE_ID.to_owned()),
    ));
    let page = plugin
        .list_usage_records(
            MeterTypeId::new(super::fixtures::CONTRACT_METER_TYPE_ID).expect("valid meter type id"),
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

// ---------------------------------------------------------------------------
// The reference backend's feed page
// ---------------------------------------------------------------------------
//
// Unit tests of the reference implementation, not contract checks. DESIGN
// section 3.3's `feed-snapshot-and-replay` check is written against the SPI
// for any backend and belongs to a later slice; nothing below is added to
// `run_all` or to a coverage constant, because none of it is an obligation
// this suite puts on a plugin. What it is for is the suite's own subject:
// the reference backend's feed answers are what a plugin author reads as an
// exemplar, and they must not rot in the interval before the check that
// covers them arrives.

/// The tenant the feed reads below withhold.
///
/// Distinct from [`super::fixtures::CONTRACT_TENANT_ID`] and from the scope
/// check's unused tenant: entries attributed to it are admitted by one grant
/// below and withheld by another, which is the only way a read can show that
/// the position does not depend on the grant.
const FEED_OTHER_TENANT_ID: Uuid = Uuid::from_u128(0xc047_c047_0000_4000_8000_0000_0000_0003);

/// The ledger's length, and so the position at its end.
const FEED_LEDGER_LEN: u64 = 4;

/// Four entries in the ledger's append order, alternating the tenant the
/// grants below pin: A, B, A, B.
///
/// Interleaving is load-bearing rather than decorative. A backend that
/// counted admitted entries instead of scanned ones would still walk a
/// ledger whose entries were all admitted, and would still finish one whose
/// withheld entries were all at the end. Alternating them means a page's
/// cursor has to jump past an entry the page did not carry, every page.
///
/// Returns the backend and the entry ids in append order, so an assertion
/// names which entries a grant should have been handed rather than
/// re-deriving an identity.
async fn feed_ledger() -> (InMemoryReferencePlugin, Vec<Uuid>) {
    let plugin = InMemoryReferencePlugin::new();
    let tenants = [
        super::fixtures::CONTRACT_TENANT_ID,
        FEED_OTHER_TENANT_ID,
        super::fixtures::CONTRACT_TENANT_ID,
        FEED_OTHER_TENANT_ID,
    ];
    assert_eq!(
        u64::try_from(tenants.len()).unwrap_or(u64::MAX),
        FEED_LEDGER_LEN,
        "`FEED_LEDGER_LEN` is the position at this ledger's end and every assertion below reads \
         it, so it must be this ledger's own length"
    );

    let mut ids = Vec::new();
    for (index, tenant_id) in tenants.into_iter().enumerate() {
        // The idempotency key is one of the five attributes the derived
        // identity reads, so varying it alone makes four entries rather than
        // one entry replayed four times.
        let key = IdempotencyKey::new(format!("feed-position-{index}"))
            .expect("the feed fixture key is well formed");
        let offset = time::Duration::hours(i64::try_from(index).unwrap_or(0) + 1);
        let record = super::fixtures::fixture_record_for_tenant(
            tenant_id,
            &key,
            UsageQuantity::parse("1").expect("fixture quantity"),
            super::fixtures::FIXTURE_EPOCH,
            super::fixtures::FIXTURE_EPOCH.saturating_add(offset),
        )
        .expect("the feed fixture is projectable");
        ids.push(record.id);
        plugin
            .create_usage_record(record)
            .await
            .expect("the reference backend admits a well-formed entry");
    }
    (plugin, ids)
}

/// The subscription every feed read below dispatches: the one meter the
/// fixture vocabulary attaches every entry to.
fn feed_subscription() -> Vec<MeterTypeId> {
    vec![
        MeterTypeId::new(super::fixtures::CONTRACT_METER_TYPE_ID)
            .expect("the suite's own meter type id is valid"),
    ]
}

/// A compiled single-tenant grant: `tenant_id eq <tenant>`.
fn tenant_scope(tenant_id: Uuid) -> ast::Expr {
    compare(
        "tenant_id",
        ast::CompareOperator::Eq,
        ast::Value::Uuid(tenant_id),
    )
}

/// A grant this backend cannot translate at all.
///
/// A function node, the same shape
/// [`an_untranslatable_node_refuses_a_caller_filter_and_excludes_a_scope`]
/// dispatches. As a scope it admits nothing — a grant that cannot be read
/// grants nothing — which is the third, widest-apart admission the position
/// has to survive.
fn untranslatable_scope() -> ast::Expr {
    ast::Expr::Function(
        "contains".to_owned(),
        vec![
            ast::Expr::Identifier("resource_id".to_owned()),
            ast::Expr::Value(ast::Value::String("contract".to_owned())),
        ],
    )
}

/// A position as this backend spells one: a scanned-entry count in eight
/// big-endian bytes.
///
/// Spelled here rather than read back from the backend's private encoder, so
/// the expected value is a statement rather than an agreement with whatever
/// the subject produced.
fn feed_position(scanned: u64) -> FeedPosition {
    FeedPosition::new(scanned.to_be_bytes().to_vec())
        .expect("eight bytes is well inside the published position bound")
}

/// The entry ids a page carries, in the order it carried them.
fn entry_ids(entries: &[UsageRecord]) -> Vec<Uuid> {
    entries.iter().map(|entry| entry.id).collect()
}

/// One feed read that must succeed.
async fn feed_page(
    plugin: &InMemoryReferencePlugin,
    scope: &ast::Expr,
    start: FeedStart<FeedPosition>,
    until: Option<FeedPosition>,
    limit: u64,
) -> FeedPage<FeedPosition> {
    plugin
        .read_feed_page(&feed_subscription(), scope, start, until, limit)
        .await
        .expect("the reference backend serves a well-formed feed read")
}

/// One ledger, three grants, one position.
///
/// This is the most valuable assertion in the file and the easiest to lose.
/// A feed position counts the entries a read **scanned**, not the ones it
/// **admitted**, so three callers whose compiled scopes differ are handed
/// the same position over one ledger and can each resume from any of them.
/// Moving the scope filter above `cursor += 1` in `read_feed_page` still
/// compiles, still passes the entire contract suite, and still reads
/// correctly under every single grant — it would simply hand three callers
/// three mutually incompatible cursors over one ledger, and a cursor minted
/// under a grant that has since widened would then silently skip entries.
///
/// A single-grant test cannot catch that; only comparing positions across
/// grants can, which is why the three reads are compared to each other
/// before any of them is compared to a literal.
///
/// The three grants are as far apart as this backend admits: one that
/// admits half the ledger, one that admits the other half, and one that
/// cannot be read and so admits none of it.
#[tokio::test]
async fn a_feed_position_is_the_same_under_every_grant_over_one_ledger() {
    let (plugin, ids) = feed_ledger().await;

    let pinned = feed_page(
        &plugin,
        &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
        FeedStart::Oldest,
        None,
        16,
    )
    .await;
    let other = feed_page(
        &plugin,
        &tenant_scope(FEED_OTHER_TENANT_ID),
        FeedStart::Oldest,
        None,
        16,
    )
    .await;
    let unreadable = feed_page(
        &plugin,
        &untranslatable_scope(),
        FeedStart::Oldest,
        None,
        16,
    )
    .await;

    // The three grants admit three different things. Without this the
    // position comparison below would be satisfied by a backend that
    // ignored the scope entirely, or by one that admitted nothing at all.
    assert_eq!(
        entry_ids(&pinned.entries),
        vec![ids[0], ids[2]],
        "the grant pinning the suite's tenant must carry that tenant's two entries, in the \
         ledger's append order, and neither of the other tenant's"
    );
    assert_eq!(
        entry_ids(&other.entries),
        vec![ids[1], ids[3]],
        "the grant pinning the other tenant must carry the complementary two entries: the scope \
         gates what the page carries"
    );
    assert!(
        unreadable.entries.is_empty(),
        "a grant this backend cannot translate admits nothing, the same fail-closed \
         disposition as the point lookup's, so its page carries no entry; it got: {:?}",
        entry_ids(&unreadable.entries)
    );

    // And they are all handed the same position.
    assert_eq!(
        pinned.next, other.next,
        "two callers whose grants admit disjoint halves of one ledger MUST be handed the same \
         position. The position counts entries scanned, not entries admitted, which is what \
         makes it independent of who asked; a position that moved with the grant could not be \
         resumed by a caller whose grant had since widened without silently skipping every \
         entry the narrower grant withheld"
    );
    assert_eq!(
        other.next, unreadable.next,
        "a grant that cannot be read admits nothing and still scans everything, so it too MUST \
         be handed the position at the ledger's end. A cursor that stalled here would pin a \
         caller at the oldest position forever the moment one conjunct of its grant became \
         untranslatable"
    );
    assert_eq!(
        pinned.next,
        Some(feed_position(FEED_LEDGER_LEN)),
        "the shared position is the count of entries scanned ({FEED_LEDGER_LEN}), not the count \
         any one grant admitted (2, 2 and 0)"
    );
}

/// A `limit`-bounded walk ends, and delivers each admitted entry once.
///
/// `limit` bounds the entries a page *carries*, so over a ledger whose
/// admitted and withheld entries alternate a page's cursor advances further
/// than the page is long. The walk below follows the cursor from `Oldest`
/// until it stops moving, which is the fixpoint at the ledger's end: an
/// empty page whose continuation is the position it was read from.
///
/// Delivering each entry exactly once is the whole point of the cursor
/// counting scanned entries. A backend that counted admitted ones would
/// re-scan every withheld entry on the following page — harmless here,
/// because a withheld entry is withheld again, and not harmless at all for
/// a caller whose grant widens between two pages.
#[tokio::test]
async fn a_limit_bounded_feed_walk_reaches_a_fixpoint_at_the_ledger_end() {
    let (plugin, ids) = feed_ledger().await;
    let scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);

    let mut start = FeedStart::Oldest;
    let mut delivered: Vec<Uuid> = Vec::new();
    let mut previous: Option<FeedPosition> = None;
    let mut reads = 0_u32;
    let fixpoint = loop {
        reads += 1;
        assert!(
            reads <= 16,
            "a walk with a page limit of one over a ledger of {FEED_LEDGER_LEN} entries must \
             reach its fixpoint in a handful of reads; {reads} says the cursor is not advancing \
             and a real gateway would be spinning here"
        );

        let FeedPage { entries, next } = feed_page(&plugin, &scope, start, None, 1).await;
        let next = next.expect(
            "a live read carries a continuation on every page, short and empty pages included: \
             an absent cursor is reserved for a bounded replay reaching its `until`",
        );
        if previous.as_ref() == Some(&next) {
            assert!(
                entries.is_empty(),
                "a page that did not move the cursor cannot have delivered anything: it would \
                 be delivering entries it is about to deliver again"
            );
            break next;
        }
        delivered.extend(entry_ids(&entries));
        previous = Some(next.clone());
        start = FeedStart::After(next);
    };

    assert_eq!(
        delivered,
        vec![ids[0], ids[2]],
        "the walk must deliver every admitted entry exactly once, in the ledger's append order. \
         A repeat means a page re-scanned what the previous page had already counted; a gap \
         means a page's cursor moved past an entry the page never carried"
    );
    assert_eq!(
        fixpoint,
        feed_position(FEED_LEDGER_LEN),
        "the fixpoint is the ledger's end, which is the number of entries scanned \
         ({FEED_LEDGER_LEN}) rather than the number delivered (2)"
    );
}

/// A bounded replay closes at the ledger's end, and only there.
///
/// An absent `next` is the one thing that says a bounded replay has reached
/// its `until` ([`crate::feed::FeedPage::next`]), so it has to be absent
/// exactly when the replay has. Bounded *past* the ledger's end the replay
/// has not reached anything yet: it keeps its cursor, and the caller resumes
/// there once the ledger has grown into the range it asked for.
#[tokio::test]
async fn a_bounded_feed_replay_closes_at_its_until_and_not_before() {
    let (plugin, ids) = feed_ledger().await;
    let scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);

    let at_the_end = feed_page(
        &plugin,
        &scope,
        FeedStart::Oldest,
        Some(feed_position(FEED_LEDGER_LEN)),
        16,
    )
    .await;
    assert_eq!(
        entry_ids(&at_the_end.entries),
        vec![ids[0], ids[2]],
        "a replay bounded at the ledger's end still carries everything the grant admits: the \
         bound is on the position, not on the page"
    );
    assert!(
        at_the_end.next.is_none(),
        "a replay whose cursor has reached its `until` is finished, and an absent `next` is \
         what says so. A position here is a page whose continuation the caller keeps \
         following, which is a hang rather than a wrong value. Got: {:?}",
        at_the_end.next
    );

    let past_the_end = feed_page(
        &plugin,
        &scope,
        FeedStart::Oldest,
        Some(feed_position(FEED_LEDGER_LEN + 5)),
        16,
    )
    .await;
    assert_eq!(
        past_the_end.next,
        Some(feed_position(FEED_LEDGER_LEN)),
        "a replay bounded past the ledger's end has not reached its `until`, so it keeps the \
         position it actually scanned to. Answering an absent cursor here would tell the caller \
         a range it never read was complete"
    );
}

/// A subscription no ledger entry answers to withholds every entry and still
/// scans the whole ledger.
///
/// The subscription is the other half of `read_feed_page`'s admission
/// decision, and the split it is on is the same one: it gates what the page
/// **carries**, never what the position **counts**. Two consumers reading
/// one backend under subscriptions of different breadth are handed
/// comparable positions for the same reason two grants are.
#[tokio::test]
async fn a_subscription_the_ledger_does_not_answer_still_advances_the_cursor() {
    let (plugin, _ids) = feed_ledger().await;
    let unsubscribed = MeterTypeId::new("gts.cf.core.uc.usage_record.v1~cf.core.uc.not_here.v1~")
        .expect("the fixture meter id is well formed");

    let page = plugin
        .read_feed_page(
            &[unsubscribed],
            &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
            FeedStart::Oldest,
            None,
            16,
        )
        .await
        .expect("a subscription naming an absent meter is a well-formed read, not a failure");

    assert!(
        page.entries.is_empty(),
        "every ledger entry carries the suite's meter, so a subscription naming another one \
         admits none of them; it got: {:?}",
        entry_ids(&page.entries)
    );
    assert_eq!(
        page.next,
        Some(feed_position(FEED_LEDGER_LEN)),
        "the position counts the {FEED_LEDGER_LEN} entries scanned even though the subscription \
         admitted none of them, exactly as it does for a grant that admits none of them"
    );
}

/// A position this backend did not issue is refused, on both paths that
/// decode one.
///
/// A plugin owns its own position encoding, so a foreign one is a
/// host-contract breach rather than a caller fault: `Internal`, never a
/// well-formed page read from a position that was guessed at. Both `start`
/// and `until` decode, so both refuse.
#[tokio::test]
async fn a_foreign_feed_position_is_refused_as_internal() {
    let (plugin, _ids) = feed_ledger().await;
    let scope = tenant_scope(super::fixtures::CONTRACT_TENANT_ID);
    let foreign = FeedPosition::new(vec![1, 2, 3])
        .expect("three bytes is an admissible position, just not one this backend issues");

    let as_a_start = plugin
        .read_feed_page(
            &feed_subscription(),
            &scope,
            FeedStart::After(foreign.clone()),
            None,
            16,
        )
        .await
        .expect_err(
            "a position of the wrong width cannot be decoded, and guessing at it would resume a \
             feed from a point nobody named",
        );
    assert!(
        matches!(
            as_a_start,
            UsageCollectorPluginError::Internal(ref detail) if detail.contains("3 bytes")
        ),
        "a foreign `start` position MUST refuse as `Internal`, and the detail must name the \
         width it was handed so an operator can see which caller minted it; got: {as_a_start:?}"
    );

    let as_an_until = plugin
        .read_feed_page(
            &feed_subscription(),
            &scope,
            FeedStart::Oldest,
            Some(foreign),
            16,
        )
        .await
        .expect_err("the `until` bound decodes through the same encoding and refuses the same way");
    assert!(
        matches!(as_an_until, UsageCollectorPluginError::Internal(_)),
        "a foreign `until` position MUST refuse as `Internal` rather than be treated as an \
         unbounded replay, which would turn a bounded read into one that never closes; got: \
         {as_an_until:?}"
    );
}

/// A zero page limit is refused.
///
/// REST enforces `minimum: 1`, so a zero limit reaching the SPI is a
/// host-contract breach. It is refused rather than answered with an empty
/// page because a zero-limit page carries nothing and so advances the cursor
/// past nothing: `next` is the position it was read from, and a caller
/// following it under an `until` never reaches it.
#[tokio::test]
async fn a_zero_limit_feed_read_is_refused_as_internal() {
    let (plugin, _ids) = feed_ledger().await;

    let refused = plugin
        .read_feed_page(
            &feed_subscription(),
            &tenant_scope(super::fixtures::CONTRACT_TENANT_ID),
            FeedStart::Oldest,
            None,
            0,
        )
        .await
        .expect_err("the published page limit is at least one, so a zero limit is malformed");

    assert!(
        matches!(refused, UsageCollectorPluginError::Internal(_)),
        "a zero limit MUST refuse as a non-retryable host-contract breach rather than be served \
         as a well-formed page that cannot advance its own cursor; got: {refused:?}"
    );
}
