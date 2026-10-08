//! The §3.11.5 conformance pin.
//!
//! Instrument **names** and the cardinality rule's **emission** side live
//! here; the cardinality rule's **source** side lives beside its sibling in
//! `domain/service_metrics_tests.rs`, and per-instrument label value sets
//! are not covered here. See the module doc on `super::metrics_inventory`
//! for why the split exists.

use std::collections::BTreeSet;

// `metrics_inventory_tests` is attached as a direct child of
// `metrics_inventory` (the `#[path]` idiom at the bottom of that file), so
// `super` names `metrics_inventory` itself — not, as the brief's sketch had
// it, a sibling to reach through a second `metrics_inventory::` segment.
use super::{DESIGN_INSTRUMENTS, InstrumentKind};
use crate::domain::ports::metrics::{
    FeedErrorCategory, RecordErrorCategory, TypeResolutionOutcome,
};

#[test]
fn the_transcription_carries_every_design_row() {
    // Guards the transcription itself: §3.11.5 declares 10 counters, 10
    // histograms and 6 gauges. A dropped row would make every assertion
    // below weaker without any of them failing.
    let counters = DESIGN_INSTRUMENTS
        .iter()
        .filter(|i| i.kind == InstrumentKind::Counter)
        .count();
    let histograms = DESIGN_INSTRUMENTS
        .iter()
        .filter(|i| i.kind == InstrumentKind::Histogram)
        .count();
    let gauges = DESIGN_INSTRUMENTS
        .iter()
        .filter(|i| i.kind == InstrumentKind::Gauge)
        .count();
    assert_eq!(
        (counters, histograms, gauges),
        (10, 10, 6),
        "DESIGN \u{a7}3.11.5 declares 10/10/6; the transcription must carry all 26",
    );

    // The 10/10/6 split alone does not guard against a duplicated row
    // silently standing in for a dropped one (same kind, same total count,
    // one fewer distinct name) — that needs the names themselves counted.
    let names: BTreeSet<&str> = DESIGN_INSTRUMENTS.iter().map(|i| i.name).collect();
    assert_eq!(
        names.len(),
        26,
        "DESIGN \u{a7}3.11.5 has 26 distinct instruments; a duplicated transcription row \
         would mask a dropped one while 10/10/6 still held",
    );
}

#[tokio::test]
async fn the_gear_emits_exactly_the_design_inventory() {
    // Wired with the real `UcMetricsMeter` over an in-memory exporter, the
    // idiom every test in `service_metrics_tests.rs` uses.
    //
    // The oracle is the document's list, never the adapter's own declarations
    // — see the module doc on H51.
    let owed: BTreeSet<&str> = DESIGN_INSTRUMENTS.iter().map(|i| i.name).collect();

    let emitted =
        crate::domain::service::service_metrics_tests::drive_every_operation_class().await;

    let missing: Vec<&&str> = owed.iter().filter(|n| !emitted.contains(**n)).collect();
    assert!(
        missing.is_empty(),
        "DESIGN §3.11.5 declares these and the gear emits none of them: {missing:?}",
    );

    // `emitted ⊆ DESIGN_INSTRUMENTS`: catches an instrument the gear emits
    // that DESIGN §3.11.5 does not declare at all.
    let all_design_names: BTreeSet<&str> = DESIGN_INSTRUMENTS.iter().map(|i| i.name).collect();
    let undocumented: Vec<&String> = emitted
        .iter()
        .filter(|n| !all_design_names.contains(n.as_str()))
        .collect();
    assert!(
        undocumented.is_empty(),
        "the gear emits these and DESIGN §3.11.5 declares none of them: {undocumented:?}",
    );
}

/// Identifiers §3.11.5's "Label cardinality" paragraph forbids as metric
/// labels outright: *"`tenant_id`, `resource_id`, `subject_id`,
/// `gts_type_id`, `request_id`, `trace_id`, idempotency keys — **must not**
/// be used as metric labels. They belong in structured logs and traces."*
const FORBIDDEN_LABEL_KEYS: &[&str] = &[
    "tenant_id",
    "resource_id",
    "subject_id",
    "gts_type_id",
    "request_id",
    "trace_id",
    "idempotency_key",
];

#[tokio::test]
async fn no_emitted_instrument_carries_an_unbounded_identifier_as_a_label() {
    // Emission side. Its own limit is stated rather than left implicit: an
    // exporter read observes only the label sets the call sites this test
    // drove happened to produce, and only for the instrument SHAPES
    // `counter_label_pairs`/`gauge_label_pairs` can read at all — see
    // `drive_every_operation_class_label_triples`'s doc for exactly which
    // shapes that excludes. The two source-side scans in
    // `service_metrics_tests.rs` —
    // `no_label_key_constant_in_ports_metrics_declares_an_unbounded_identifier`
    // (load-bearing) and
    // `no_metrics_recorder_method_is_passed_an_unbounded_identifier_as_a_label`
    // (corroborating) — are what cover the rest.
    let triples =
        crate::domain::service::service_metrics_tests::drive_every_operation_class_label_triples()
            .await;
    for (instrument, key, value) in triples {
        assert!(
            !FORBIDDEN_LABEL_KEYS.contains(&key.as_str()),
            "`{instrument}` carries label `{key}={value}`; DESIGN \u{a7}3.11.5 forbids \
             `{key}` as a metric label and routes it to logs and traces instead",
        );
    }
}

/// Every [`TypeResolutionOutcome`] variant, consumed by both tests below so
/// there is exactly one array to keep in sync with the enum, not two.
///
/// This array's own length does **not** self-update when a variant is
/// added — no Rust array literal does. What keeps it honest is
/// [`every_type_resolution_outcome_is_covered_by_the_vocabulary_test`]'s
/// exhaustive `match`, which fails to compile on a seventh variant; see that
/// test's own doc for exactly what that compile error does and does not
/// guarantee about this array.
const ALL_TYPE_RESOLUTION_OUTCOMES: [TypeResolutionOutcome; 6] = [
    TypeResolutionOutcome::CacheHit,
    TypeResolutionOutcome::CacheMiss,
    TypeResolutionOutcome::ServedStale,
    TypeResolutionOutcome::Restored,
    TypeResolutionOutcome::Unresolved,
    TypeResolutionOutcome::RegistryError,
];

/// Pins the `result` label vocabulary for `uc_type_resolution_total`, so a
/// label value emitted by the code must be declared by the document.
#[test]
fn the_type_resolution_result_vocabulary_matches_design() {
    let declared: Vec<&str> = DESIGN_INSTRUMENTS
        .iter()
        .find(|i| i.name == "uc_type_resolution_total")
        .expect("uc_type_resolution_total is a DESIGN \u{a7}3.11.5 counter")
        .labels
        .iter()
        .find(|(k, _)| *k == "result")
        .expect("its `result` label is declared")
        .1
        .to_vec();

    for outcome in ALL_TYPE_RESOLUTION_OUTCOMES {
        let emitted = outcome.as_str();
        assert!(
            declared.contains(&emitted),
            "`{emitted}` is emitted on uc_type_resolution_total and DESIGN \u{a7}3.11.5 \
             does not declare it; declared set is {declared:?}",
        );
    }

    // `restored` IS emitted now: slice 8b's Task 4 added the variant and its
    // label spelling per ruling I1, and Task 5 built `TypeResolver::rehydrate`'s
    // success path, which `populate`'s definite-not-found arm reaches. So the
    // loop above now covers `restored` like every other value, and this
    // assertion is no longer guarding an unemitted label.
    // This assertion's job has changed, but NOT to a redundant one -- an
    // earlier version of this comment claimed the loop above already
    // covers `restored` via the widened `ALL_TYPE_RESOLUTION_OUTCOMES`, so
    // this standalone check was "redundant with it". That is false, and
    // measured false: drift `TypeResolutionOutcome::Restored::as_str()` and
    // this row's transcribed spelling TOGETHER, from "restored" to
    // "restore" (the same wrong rename on both the enum and the hand-kept
    // transcription), and the loop above still passes -- it only compares
    // `outcome.as_str()` against `declared`, and both sides now agree on
    // the new, wrong spelling. The run reds ONLY here, on this assertion's
    // hard-coded literal `"restored"`, because this is the one place in
    // the module that does not derive its expectation from either side of
    // that drift. This is therefore the CO-DRIFT PIN: the sole check that
    // survives the enum and the transcription moving together, which the
    // loop above structurally cannot be, since it reads both of its
    // operands from the two things it is supposed to be guarding against
    // drifting. Keep it for that reason, not because it overlaps the loop.
    assert!(
        declared.contains(&"restored"),
        "DESIGN \u{a7}3.11.5 declares `restored` for the \u{a7}3.7 restore path; it must stay \
         declared under exactly that spelling, independently of whatever spelling \
         TypeResolutionOutcome::Restored::as_str() happens to carry -- this is the \
         co-drift pin, not the loop above",
    );
}

/// Forces a revisit here when [`TypeResolutionOutcome`] grows a variant —
/// no stronger claim than that.
///
/// The exhaustive `match` below is a compile error on a seventh variant, so
/// adding one cannot ship silently *unnoticed by this function*. What it
/// does **not** do is reach into [`ALL_TYPE_RESOLUTION_OUTCOMES`] or the
/// vocabulary test above and extend either for you — a human still has to
/// widen both `covered`'s match arms and that array by hand. The array
/// being shared between the two tests is what the fix round F11 found:
/// before, the vocabulary test's loop was its own independently-written
/// five-element literal that this compile error did nothing to touch, so
/// "fixing the compile error" and "updating the vocabulary test" were two
/// unlinked edits one could do without the other. Sharing the array here
/// does not make either test exhaustive; it makes forgetting one of them
/// impossible once you have already come here to fix the other.
///
/// **That is weaker than it reads, and Slice 8b Task 4's fix round measured
/// exactly how weak.** A seventh variant's compile error (`E0004`) only
/// obliges `covered`'s match to grow an arm — it says nothing about that
/// arm's body or about this array. Proof: add a seventh variant, fix *only*
/// the four sites the compiler names (this match included, with a trivial
/// `=> true` arm), and leave [`ALL_TYPE_RESOLUTION_OUTCOMES`] at its old
/// length. The crate compiles and the full lib lane passes with no new
/// failure, because the `for o in ALL_TYPE_RESOLUTION_OUTCOMES` loop below
/// simply never produces the omitted variant to feed to `covered` — nothing
/// here calls it on a value the array doesn't already contain. Rewriting
/// each arm's body to `ALL_TYPE_RESOLUTION_OUTCOMES.contains(&o)` (checking
/// membership instead of returning a bare `true`) does not change this: the
/// check is tautological when its input is drawn from the very array it is
/// checking membership in. Making the array itself compiler-derived from
/// the enum (so there is one hand-maintained list instead of two) would
/// need either a proc-macro that walks the enum's variants or an external
/// crate (`strum::EnumIter` or similar); this crate's own `#[domain_model]`
/// derive does not provide one, and adding either is more machinery than a
/// single six-element array justifies. So the honest claim is proximity,
/// not enforcement: the compile error and the array edit arrive in the same
/// sitting because a human who has just been forced to touch this function
/// is already looking at the array two lines below it — not because
/// anything stops them leaving after fixing only the match.
#[test]
fn every_type_resolution_outcome_is_covered_by_the_vocabulary_test() {
    fn covered(o: TypeResolutionOutcome) -> bool {
        match o {
            TypeResolutionOutcome::CacheHit
            | TypeResolutionOutcome::CacheMiss
            | TypeResolutionOutcome::ServedStale
            | TypeResolutionOutcome::Restored
            | TypeResolutionOutcome::Unresolved
            | TypeResolutionOutcome::RegistryError => true,
        }
    }
    for o in ALL_TYPE_RESOLUTION_OUTCOMES {
        assert!(covered(o));
    }
}

/// Every [`RecordErrorCategory`] variant, consumed by the two tests below —
/// the same shape [`ALL_TYPE_RESOLUTION_OUTCOMES`] establishes, and
/// deliberately a *second*, independently-maintained enumeration from
/// `infra::metrics::metrics_tests::ALL_RECORD_ERROR_CATEGORIES`, which
/// pairs each variant with a hand-written literal for a different purpose
/// (pinning `as_str` itself, fix round F1). This array needs no literal
/// spelling field: the test below reads `as_str()` directly, because what
/// it checks is drift between `DESIGN_INSTRUMENTS` and the enum, not
/// whether `as_str()` itself is spelled correctly.
const ALL_RECORD_ERROR_CATEGORY_VARIANTS: [RecordErrorCategory; 8] = [
    RecordErrorCategory::None,
    RecordErrorCategory::Authz,
    RecordErrorCategory::UnknownUsageType,
    RecordErrorCategory::SemanticsViolation,
    RecordErrorCategory::InvalidationRule,
    RecordErrorCategory::MetadataSize,
    RecordErrorCategory::IdempotencyConflict,
    RecordErrorCategory::PluginError,
];

/// Fix round F11 (round 2). Pins that every value
/// [`RecordErrorCategory::as_str`] can produce is one [`DESIGN_INSTRUMENTS`]
/// declares for `uc_ingestion_records_total`. Before this test, nothing compared the
/// two sides — `infra::metrics::metrics_tests::ALL_RECORD_ERROR_CATEGORIES`
/// pins the enum against hand-written literals (it caught the fix round's
/// Critical, F1), but those literals are independent of this file's
/// transcription, so a typo in `DESIGN_INSTRUMENTS` itself passed
/// unnoticed. Measured: replacing `"unknown_usage_type"` with
/// `"probe_bogus_value"` in this row below left 815 tests passing.
#[test]
fn the_record_error_category_vocabulary_matches_design() {
    let declared: Vec<&str> = DESIGN_INSTRUMENTS
        .iter()
        .find(|i| i.name == "uc_ingestion_records_total")
        .expect("uc_ingestion_records_total is a DESIGN \u{a7}3.11.5 counter")
        .labels
        .iter()
        .find(|(k, _)| *k == "error_category")
        .expect("its `error_category` label is declared")
        .1
        .to_vec();

    for category in ALL_RECORD_ERROR_CATEGORY_VARIANTS {
        let emitted = category.as_str();
        assert!(
            declared.contains(&emitted),
            "`{emitted}` is emitted on uc_ingestion_records_total and DESIGN \u{a7}3.11.5 \
             does not declare it; declared set is {declared:?}",
        );
    }

    // `unresolved_type` and `validation` are declared but unreachable on
    // this counter today (DESIGN.md:2239; open and unassigned). Asserted so
    // neither is deleted as dead by a
    // future pass that reads only the emitted side.
    for unreachable in ["unresolved_type", "validation"] {
        assert!(
            declared.contains(&unreachable),
            "DESIGN \u{a7}3.11.5 declares `{unreachable}` on uc_ingestion_records_total \
             it must stay declared even though \
             this gear emits it nowhere",
        );
    }
}

/// Forces a revisit here when [`RecordErrorCategory`] grows a variant — no
/// stronger claim than that. See
/// [`every_type_resolution_outcome_is_covered_by_the_vocabulary_test`]'s own
/// doc for exactly what the exhaustive-`match` idiom does and does not
/// guarantee about [`ALL_RECORD_ERROR_CATEGORY_VARIANTS`].
#[test]
fn every_record_error_category_variant_is_covered_by_the_design_vocabulary_test() {
    fn covered(c: RecordErrorCategory) -> bool {
        match c {
            RecordErrorCategory::None
            | RecordErrorCategory::Authz
            | RecordErrorCategory::UnknownUsageType
            | RecordErrorCategory::SemanticsViolation
            | RecordErrorCategory::InvalidationRule
            | RecordErrorCategory::MetadataSize
            | RecordErrorCategory::IdempotencyConflict
            | RecordErrorCategory::PluginError => true,
        }
    }
    for c in ALL_RECORD_ERROR_CATEGORY_VARIANTS {
        assert!(covered(c));
    }
}

/// Every [`FeedErrorCategory`] variant, consumed by the two tests below —
/// see [`ALL_RECORD_ERROR_CATEGORY_VARIANTS`]'s own doc for why this is a
/// deliberate second enumeration rather than a reuse of
/// `infra::metrics::metrics_tests::ALL_FEED_ERROR_CATEGORIES`.
const ALL_FEED_ERROR_CATEGORY_VARIANTS: [FeedErrorCategory; 6] = [
    FeedErrorCategory::None,
    FeedErrorCategory::Authz,
    FeedErrorCategory::CursorDecode,
    FeedErrorCategory::CursorBeyondRetention,
    FeedErrorCategory::ArgumentRejected,
    FeedErrorCategory::PluginError,
];

/// Fix round F11 (round 2), the feed half. Pins that every value
/// [`FeedErrorCategory::as_str`] can produce is one [`DESIGN_INSTRUMENTS`]
/// declares for `uc_feed_requests_total`. See
/// [`the_record_error_category_vocabulary_matches_design`]'s own doc for
/// the shape of the gap this closes.
#[test]
fn the_feed_error_category_vocabulary_matches_design() {
    let declared: Vec<&str> = DESIGN_INSTRUMENTS
        .iter()
        .find(|i| i.name == "uc_feed_requests_total")
        .expect("uc_feed_requests_total is a DESIGN \u{a7}3.11.5 counter")
        .labels
        .iter()
        .find(|(k, _)| *k == "error_category")
        .expect("its `error_category` label is declared")
        .1
        .to_vec();

    for category in ALL_FEED_ERROR_CATEGORY_VARIANTS {
        let emitted = category.as_str();
        assert!(
            declared.contains(&emitted),
            "`{emitted}` is emitted on uc_feed_requests_total and DESIGN \u{a7}3.11.5 \
             does not declare it; declared set is {declared:?}",
        );
    }
}

/// Forces a revisit here when [`FeedErrorCategory`] grows a variant — no
/// stronger claim than that. See
/// [`every_type_resolution_outcome_is_covered_by_the_vocabulary_test`]'s own
/// doc for exactly what the exhaustive-`match` idiom does and does not
/// guarantee about [`ALL_FEED_ERROR_CATEGORY_VARIANTS`].
#[test]
fn every_feed_error_category_variant_is_covered_by_the_design_vocabulary_test() {
    fn covered(c: FeedErrorCategory) -> bool {
        match c {
            FeedErrorCategory::None
            | FeedErrorCategory::Authz
            | FeedErrorCategory::CursorDecode
            | FeedErrorCategory::CursorBeyondRetention
            | FeedErrorCategory::ArgumentRejected
            | FeedErrorCategory::PluginError => true,
        }
    }
    for c in ALL_FEED_ERROR_CATEGORY_VARIANTS {
        assert!(covered(c));
    }
}
