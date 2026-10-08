//! Unit tests for the [`UsageCollectorError`] constructors whose message text
//! is itself normative, plus the document-versus-code pins for DESIGN §3.3's
//! Error Contract.
//!
//! Most constructors carry prose no document constrains, and pinning that would
//! freeze wording for its own sake. The ones below are different: DESIGN §3.1's
//! Target resolution row and
//! `cpt-cf-usage-collector-adr-append-only-invalidation`'s Valid target rule
//! both say what an unresolvable target's message has to tell the emitter.
//!
//! The tests read `docs/DESIGN.md` and `docs/usage-collector-v1.yaml` from disk
//! rather than transcribing either, via
//! `std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/…"))`
//! — `CARGO_MANIFEST_DIR` is this crate's own root at compile time, so the path
//! resolves the same way whatever directory `cargo test` runs from. Every read
//! panics naming the path and the `io::Error`: an unreadable document must not
//! read as a silent pass.

use super::*;

/// The target identifier is derived, never sent, so the rejection has to name
/// the inputs it was derived from.
///
/// DESIGN §3.1, Target resolution: nothing found is `NotFound`, "whose message
/// says the target is identified by tenant, GTS type, idempotency key, and
/// covered period, since a typo in any of them surfaces there rather than as a
/// field mismatch". ADR 0010's Valid target rule says the same.
#[test]
fn the_unresolvable_target_rejection_names_the_four_locating_inputs() {
    let target = Uuid::from_u128(0x0BAD_1DEA);
    let err = UsageCollectorError::invalidation_target_not_found(target);
    let UsageCollectorError::NotFound {
        reason,
        name,
        detail,
        ..
    } = &err
    else {
        panic!("expected NotFound, got {err:?}");
    };
    assert_eq!(*reason, NotFoundReason::InvalidationTargetNotFound);
    assert_eq!(name, &target.to_string());

    // Each locating input is asserted separately, so a message that drops one
    // fails on that one rather than on an opaque whole-string comparison.
    for input in ["tenant", "GTS type", "idempotency key", "covered period"] {
        assert!(
            detail.contains(input),
            "the rejection must name `{input}` as part of what identifies the target; got {detail}"
        );
    }

    // The derived identifier stays, for an operator correlating the 404
    // against a log line or against `name`.
    assert!(detail.contains(&target.to_string()), "{detail}");
}

/// The message must not send the emitter after `invalidates`.
///
/// `invalidates` is server-assigned (DESIGN §3.1, Field ownership) and
/// [`crate::CreateUsageRecord`] declares no such property, so an emitter told
/// the rejection is about `invalidates` looks for a field they had no way to
/// send. This is the half a `contains` check on the locating inputs cannot
/// catch: a message can name them all and still lead with the wrong noun.
#[test]
fn the_unresolvable_target_rejection_names_no_field_the_emitter_never_sent() {
    let err = UsageCollectorError::invalidation_target_not_found(Uuid::from_u128(7));
    let UsageCollectorError::NotFound { detail, .. } = &err else {
        panic!("expected NotFound, got {err:?}");
    };
    assert!(
        !detail.contains("invalidates"),
        "`invalidates` is server-assigned and absent from the ingestion shape, so the \
         rejection must not present it as the caller's field; got {detail}"
    );
}

/// A quota rejection is a 429 (`ResourceExhausted`) carrying an integer retry
/// delay and a detail naming both the allowance and the submitted count.
///
/// A detail assertion, not a variant assertion (spec §7 / §15.4 rule 3): an
/// operator reading one envelope must be able to tell a burst from a sustained
/// overrun, so both numbers appear in `detail` rather than only the variant
/// discriminating the outcome.
#[test]
fn a_quota_rejection_is_a_429_carrying_an_integer_retry_delay() {
    let err = UsageCollectorError::ingestion_quota_exceeded(IngestionQuotaExceededArgs {
        allowance: 4000,
        submitted: 5000,
        retry_after_seconds: 1,
    });
    match &err {
        UsageCollectorError::ResourceExhausted {
            retry_after_seconds,
            detail,
        } => {
            assert_eq!(*retry_after_seconds, 1);
            assert!(detail.contains("4000"), "allowance in detail: {detail}");
            assert!(
                detail.contains("5000"),
                "submitted count in detail: {detail}"
            );
        }
        other => panic!("expected ResourceExhausted, got {other:?}"),
    }
}

/// DESIGN §3.3 Error Contract: `retry_after()` is "the complete retry decision"
/// and must answer `Some` for a quota rejection, one of the outcomes it names
/// retryable.
#[test]
fn retry_after_is_some_for_a_quota_rejection() {
    let err = UsageCollectorError::ingestion_quota_exceeded(IngestionQuotaExceededArgs {
        allowance: 4000,
        submitted: 5000,
        retry_after_seconds: 7,
    });
    assert_eq!(err.retry_after(), Some(std::time::Duration::from_secs(7)));
}

/// `ServiceUnavailable` carrying a plugin-supplied hint answers `Some` —
/// another of DESIGN §3.3's retryable outcomes.
#[test]
fn retry_after_is_some_for_service_unavailable_s_hint() {
    let err = UsageCollectorError::service_unavailable("downstream reset", Some(30));
    assert_eq!(err.retry_after(), Some(std::time::Duration::from_secs(30)));
}

/// A bare, config-free `ServiceUnavailable` without a plugin-supplied hint
/// still answers `None`: this crate carries no configuration and cannot
/// synthesize a value it was never given. **Where it is given one, it answers
/// `Some`** — that half is
/// [`retry_after_is_some_for_plugin_unavailable_with_the_configured_default`]
/// below.
#[test]
fn retry_after_is_none_without_a_hint() {
    assert_eq!(
        UsageCollectorError::plugin_unavailable(None).retry_after(),
        None
    );
}

/// The other half: `plugin_unavailable`'s `Option<u64>` parameter reaches
/// `retry_after()` exactly like [`UsageCollectorError::service_unavailable`]'s
/// does — supplying `Some` here is indistinguishable, from this crate's
/// perspective, from a plugin-supplied hint. The host's
/// `domain::error::lift_domain_error` supplies the configured
/// `unavailable_retry_after_secs` default this way, for every dispatch that
/// reaches no plugin.
#[test]
fn retry_after_is_some_for_plugin_unavailable_with_the_configured_default() {
    assert_eq!(
        UsageCollectorError::plugin_unavailable(Some(7)).retry_after(),
        Some(std::time::Duration::from_secs(7))
    );
}

/// A bare, config-free `Conflict(TargetNotConverged)` still answers `None`:
/// this crate carries no configuration of its own. **Where the host supplies the
/// configured `target_not_converged_retry_after_secs` delay, it answers `Some`**
/// — that half is
/// [`retry_after_is_some_for_target_not_converged_with_the_configured_default`]
/// below.
#[test]
fn retry_after_is_none_for_target_not_converged_the_sdk_variant_carries_no_delay() {
    let err = UsageCollectorError::target_not_converged(Uuid::from_u128(1), None);
    assert_eq!(err.retry_after(), None);
}

/// The other half: see
/// [`retry_after_is_none_for_target_not_converged_the_sdk_variant_carries_no_delay`].
/// The host's `domain::error::lift_domain_error` supplies the configured
/// `target_not_converged_retry_after_secs` default this way, for every
/// `DomainError::TargetNotConverged`.
#[test]
fn retry_after_is_some_for_target_not_converged_with_the_configured_default() {
    let err = UsageCollectorError::target_not_converged(Uuid::from_u128(1), Some(3));
    assert_eq!(err.retry_after(), Some(std::time::Duration::from_secs(3)));
}

/// A category DESIGN §3.3 never calls retryable answers `None`.
#[test]
fn retry_after_is_none_for_a_non_retryable_category() {
    assert_eq!(
        UsageCollectorError::permission_denied("denied").retry_after(),
        None,
    );
}

// ── Step 2/3: DESIGN §3.3's Error Contract table vs. the enum ──────────

/// DESIGN §3.3's Error Contract table must carry exactly one row per
/// [`UsageCollectorError`] variant — no fewer, no more, and every variant named
/// exactly once.
///
/// The divergence this closes: the table named neither `CursorRejected` nor
/// `AlreadyExists`, and nothing compared the two, so the gap went unnoticed.
///
/// `variant_name` is the pin's load-bearing half: an exhaustive match with no
/// wildcard arm, over a real [`UsageCollectorError`] value for every variant.
/// `UsageCollectorError` is `#[non_exhaustive]`, but that attribute only forces
/// a wildcard in a *different* crate — this module is a child of `error` itself,
/// so a new variant added anywhere in this crate makes this match fail to
/// compile before the test can run. That is what makes this a pin rather than a
/// snapshot.
#[test]
fn design_doc_error_contract_table_has_one_row_per_variant() {
    const DESIGN_MD_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/DESIGN.md");
    const TABLE_HEADER: &str = "| Variant | AIP-193 category | HTTP | Raised for |";

    /// Exhaustive, wildcard-free: see this test's doc for why a tenth
    /// variant fails the *build*, not just this assertion.
    fn variant_name(e: &UsageCollectorError) -> &'static str {
        match e {
            UsageCollectorError::PermissionDenied { .. } => "PermissionDenied",
            UsageCollectorError::InvalidArgument { .. } => "InvalidArgument",
            UsageCollectorError::CursorRejected { .. } => "CursorRejected",
            UsageCollectorError::NotFound { .. } => "NotFound",
            UsageCollectorError::AlreadyExists { .. } => "AlreadyExists",
            UsageCollectorError::Conflict { .. } => "Conflict",
            UsageCollectorError::ServiceUnavailable { .. } => "ServiceUnavailable",
            UsageCollectorError::ResourceExhausted { .. } => "ResourceExhausted",
            UsageCollectorError::Internal { .. } => "Internal",
        }
    }

    let design = std::fs::read_to_string(DESIGN_MD_PATH)
        .unwrap_or_else(|e| panic!("DESIGN.md must be readable at {DESIGN_MD_PATH}: {e}"));

    let header_at = design.find(TABLE_HEADER).unwrap_or_else(|| {
        panic!(
            "DESIGN.md's §3.3 Error Contract table header {TABLE_HEADER:?} was not found; it \
             was renamed, reworded, or the section moved, and this pin has to move with it"
        )
    });

    // Collect every `| ... |` row after the header and its `| --- | ... |`
    // separator, stopping at the first line that is not a table row — the
    // table's trailing blank line.
    let mut lines = design[header_at..].lines();
    lines.next(); // the header row itself
    lines.next(); // the `| --- | --- | --- | --- |` separator
    let documented_variants: Vec<String> = lines
        .take_while(|line| line.starts_with('|'))
        .map(|line| {
            line.split('|')
                .nth(1)
                .unwrap_or_default()
                .trim()
                .trim_matches('`')
                .to_owned()
        })
        .collect();

    // One concrete value per variant, so `variant_name` actually runs over each
    // rather than existing only to satisfy the compiler. `AlreadyExists` has no
    // constructor, so it is built directly — its fields are private but this
    // module is `error`'s own child.
    let one_of_each: [UsageCollectorError; 9] = [
        UsageCollectorError::permission_denied("x"),
        UsageCollectorError::invalid_batch_size(0, 1, 2),
        UsageCollectorError::cursor_query_mismatch(),
        UsageCollectorError::usage_record_not_found(Uuid::nil()),
        UsageCollectorError::AlreadyExists {
            resource_type: "x".to_owned(),
            name: "x".to_owned(),
            detail: "x".to_owned(),
        },
        UsageCollectorError::idempotency_conflict("key", Uuid::nil()),
        UsageCollectorError::plugin_unavailable(None),
        UsageCollectorError::ingestion_quota_exceeded(IngestionQuotaExceededArgs {
            allowance: 1,
            submitted: 1,
            retry_after_seconds: 1,
        }),
        UsageCollectorError::internal("x"),
    ];
    let enum_variants: std::collections::BTreeSet<&'static str> =
        one_of_each.iter().map(variant_name).collect();

    let documented_set: std::collections::BTreeSet<String> =
        documented_variants.iter().cloned().collect();
    let enum_set: std::collections::BTreeSet<String> =
        enum_variants.iter().map(|s| (*s).to_owned()).collect();

    let variants_with_no_table_row: Vec<&String> = enum_set.difference(&documented_set).collect();
    let table_rows_naming_no_variant: Vec<&String> = documented_set.difference(&enum_set).collect();

    assert!(
        variants_with_no_table_row.is_empty() && table_rows_naming_no_variant.is_empty(),
        "DESIGN.md §3.3's Error Contract table and `UsageCollectorError`'s variants have \
         drifted apart: variant(s) {variants_with_no_table_row:?} have no table row, and \
         table row(s) {table_rows_naming_no_variant:?} name no current variant"
    );

    // The set comparison above would miss a duplicated row that still covered
    // every variant, so the row count is pinned exactly too.
    assert_eq!(
        documented_variants.len(),
        enum_set.len(),
        "the table must carry exactly one row per variant: found {} row(s) for {} \
         variant(s); a duplicate row would pass the set comparison above and fail only here",
        documented_variants.len(),
        enum_set.len(),
    );
}

// ── Step 4: the reason vocabularies against `usage-collector-v1.yaml` ──

/// Every [`ConflictReason`] variant's wire string is published in
/// `usage-collector-v1.yaml`, wildcard-free over the variants.
///
/// `Unknown(String)` — `from_wire`'s final arm — is excused from this pin, named
/// and reasoned rather than swallowed by a wildcard: it carries no fixed wire
/// string of its own (it *is* the raw wire string, verbatim, for whatever a
/// future peer sends that this crate does not model), and it is the mechanism
/// that makes DESIGN §3.3's "additive within a major version" claim safe. An
/// excused arm with a stated reason, not a wildcard: a new variant still has to
/// be placed in one arm or the other before this compiles.
#[test]
fn every_conflict_reason_wire_string_is_published_in_the_yaml() {
    const YAML_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/usage-collector-v1.yaml"
    );

    fn wire_string(reason: &ConflictReason) -> Option<&str> {
        match reason {
            ConflictReason::IdempotencyConflict => Some(crate::reason::IDEMPOTENCY_CONFLICT),
            ConflictReason::AlreadyInvalidated => Some(crate::reason::ALREADY_INVALIDATED),
            ConflictReason::TargetNotConverged => Some(crate::reason::TARGET_NOT_CONVERGED),
            // Excused, not wildcarded — see this test's doc.
            ConflictReason::Unknown(_) => None,
        }
    }

    let yaml = std::fs::read_to_string(YAML_PATH)
        .unwrap_or_else(|e| panic!("usage-collector-v1.yaml must be readable at {YAML_PATH}: {e}"));

    for reason in [
        ConflictReason::IdempotencyConflict,
        ConflictReason::AlreadyInvalidated,
        ConflictReason::TargetNotConverged,
    ] {
        let wire = wire_string(&reason).expect("the three modeled variants all carry a string");
        assert!(
            yaml.contains(wire),
            "`ConflictReason::{reason:?}`'s wire string `{wire}` must appear in \
             usage-collector-v1.yaml: DESIGN §3.3 says the reason vocabularies are published \
             and additive, so a modeled reason with no trace in the yaml is either undocumented \
             or has drifted from its published spelling"
        );
    }
}

/// Every [`ValidationReason`] variant's wire string, asserted present in
/// `usage-collector-v1.yaml` — **measured**, not assumed.
///
/// `Unknown(String)`, `from_wire`'s final arm, is excused on its own footing —
/// see [`Published::NoFixedString`] — for the same reason as
/// `ConflictReason::Unknown` above: it carries no fixed string to check, and it
/// is what keeps "additive within a major version" safe.
///
/// **The yaml did not always publish them all**, and this test held the gap open
/// as a live two-sided assertion — `Yes(wire)` asserting presence, `No(wire)`
/// asserting absence — rather than as a comment that could rot. The gap is
/// closed and the `No` disposition is gone with it, so this is now a plain
/// completeness pin: every modeled reason a consumer can decode is a reason the
/// published contract names. Closing it took two kinds of publication, because
/// the absent codes were never one population — a distinction measured from
/// raise sites, not inferred from the names:
///
/// **Live rejections this gear raises** are published next to the behavior each
/// describes, in the style the yaml already used for `INVALIDATION_FIELD_MISMATCH`:
/// `QUANTITY_OUT_OF_RANGE` on `UsageQuantity`, and `METADATA_VALIDATION`,
/// `UNKNOWN_METADATA_KEY`, `INVALID_BASE_GTS_ID` on `RejectedUsageRecord`'s
/// rejection catalogue, each naming the `field` it is attributed to.
///
/// **Codes with no production raise site anywhere in the repository** are
/// published as **reserved** on `Problem.context` — named so a consumer decoding
/// a stored or foreign envelope can resolve them, and stated in the same breath
/// to be codes this version never emits. Publishing them as live rejections
/// would have asserted behavior the gear does not have, which is the `PastWindow`
/// trap below. They are:
///
/// - `SEMANTICS_VIOLATION`, retired. [`crate::reason::SEMANTICS_VIOLATION`] says
///   it outright — "emitted by nothing in this crate". It named a value-matrix
///   rule keyed on a quantity's sign, which `UsageQuantity` forbids, so its
///   constructors are gone and the variant survives only so a consumer can model
///   an envelope stored while the rule was live.
/// - `INVALID_METADATA_FIELDS_EMPTY_STRING`,
///   `INVALID_METADATA_FIELDS_INVALID_KEY`, and
///   `INVALID_METADATA_FIELDS_DUPLICATE`, which report a malformed entry in a
///   **meter type declaration's** `metadata_fields` list. DESIGN §3.2 puts that
///   surface outside this gear — a declaration is "**Not an entity of this
///   gear**" — and `error.rs`'s `repeated_grouping_dimension` already refuses to
///   borrow `MetadataFieldDuplicate` for a repeated `group_by` dimension on
///   exactly that ground.
///
/// **`PastWindow` is pinned here as published, deliberately not asserting the
/// converse.** `usage-collector-v1.yaml` names `PAST_WINDOW` at the backfill
/// route's description as the hard backfill-window rejection's reason — and in
/// the same paragraph states outright that this rejection "does not exist in the
/// gear" (owner-reserved divergence G23, spec §11 item 1). The string is
/// genuinely present, so the one-directional claim this test makes holds. The
/// converse — every published reason describes behavior the gear has — does not,
/// and is not asserted: a pin written the other way would red on G23's reserved
/// item and on the reserved codes above, reading as a bug in each rather than as
/// the deliberate publication it is.
#[test]
fn every_validation_reason_wire_string_the_yaml_documents_matches_the_constant() {
    const YAML_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/usage-collector-v1.yaml"
    );

    /// Every modeled `ValidationReason` variant's disposition against the
    /// published yaml, measured rather than assumed. Wildcard-free: a new
    /// variant (or `Unknown`, excused by name) must be placed here
    /// deliberately.
    enum Published {
        /// The yaml names this wire string somewhere in its prose. Every
        /// modeled variant now carries this disposition — see this test's doc
        /// for what closing the gap required and why the `No` disposition this
        /// enum used to carry is gone.
        Yes(&'static str),
        /// No fixed wire string exists to check at all — `Unknown(String)`
        /// alone. Distinct from `Yes`: that disposition makes a falsifiable
        /// claim about a real string, this one has no string to make a
        /// claim with.
        NoFixedString,
    }

    fn disposition(reason: &ValidationReason) -> Published {
        use crate::reason::*;
        match reason {
            ValidationReason::Validation => Published::Yes(VALIDATION),
            ValidationReason::AggregationResultTooLarge => {
                Published::Yes(AGGREGATION_RESULT_TOO_LARGE)
            }
            ValidationReason::FutureWindow => Published::Yes(FUTURE_WINDOW),
            // Published, deliberately not asserted the other way — see
            // this test's doc on G23.
            ValidationReason::PastWindow => Published::Yes(PAST_WINDOW),
            ValidationReason::CursorBeyondRetention => Published::Yes(CURSOR_BEYOND_RETENTION),
            // Published as a rejection reason on the operation that raises
            // each, in the yaml's established style — the behavior
            // sentence names the code and the `field` it is attributed to.
            ValidationReason::MetadataValidation => Published::Yes(METADATA_VALIDATION),
            ValidationReason::UnknownMetadataKey => Published::Yes(UNKNOWN_METADATA_KEY),
            ValidationReason::InvalidBaseGtsId => Published::Yes(INVALID_BASE_GTS_ID),
            ValidationReason::QuantityOutOfRange => Published::Yes(QUANTITY_OUT_OF_RANGE),
            // Published as **reserved**, not as a rejection this gear raises:
            // no production raise site exists for any of these. The yaml names
            // them so a consumer decoding a stored or foreign envelope can
            // resolve the code, and says in the same breath that this version
            // never emits them.
            ValidationReason::SemanticsViolation => Published::Yes(SEMANTICS_VIOLATION),
            ValidationReason::MetadataFieldEmptyString => {
                Published::Yes(INVALID_METADATA_FIELDS_EMPTY_STRING)
            }
            ValidationReason::MetadataFieldInvalidKey => {
                Published::Yes(INVALID_METADATA_FIELDS_INVALID_KEY)
            }
            ValidationReason::MetadataFieldDuplicate => {
                Published::Yes(INVALID_METADATA_FIELDS_DUPLICATE)
            }
            ValidationReason::InvalidationFieldMismatch => {
                Published::Yes(INVALIDATION_FIELD_MISMATCH)
            }
            // Excused, not wildcarded — see this test's doc: no fixed
            // string exists here to assert present or absent.
            ValidationReason::Unknown(_) => Published::NoFixedString,
        }
    }

    let yaml = std::fs::read_to_string(YAML_PATH)
        .unwrap_or_else(|e| panic!("usage-collector-v1.yaml must be readable at {YAML_PATH}: {e}"));

    for reason in [
        ValidationReason::SemanticsViolation,
        ValidationReason::Validation,
        ValidationReason::MetadataValidation,
        ValidationReason::UnknownMetadataKey,
        ValidationReason::InvalidBaseGtsId,
        ValidationReason::MetadataFieldEmptyString,
        ValidationReason::MetadataFieldInvalidKey,
        ValidationReason::MetadataFieldDuplicate,
        ValidationReason::AggregationResultTooLarge,
        ValidationReason::InvalidationFieldMismatch,
        ValidationReason::FutureWindow,
        ValidationReason::PastWindow,
        ValidationReason::QuantityOutOfRange,
        ValidationReason::CursorBeyondRetention,
    ] {
        match disposition(&reason) {
            Published::Yes(wire) => {
                assert_eq!(
                    wire,
                    reason.as_wire(),
                    "the published disposition table above must quote the constant's own value"
                );
                assert!(
                    yaml.contains(wire),
                    "`ValidationReason::{reason:?}`'s wire string `{wire}` must appear in \
                     usage-collector-v1.yaml. All fourteen modeled reasons are published there, \
                     so this is a regression, not a disposition \
                     to re-measure: either the yaml dropped the code or the two sides have \
                     drifted on its spelling. Re-publish it rather than relaxing this pin — a \
                     reason a consumer can decode but cannot look up is exactly what entry 35 \
                     existed to record"
                );
            }
            Published::NoFixedString => {
                // `Unknown(_)` only; nothing to assert. See this test's doc.
            }
        }
    }
}

/// [`NotFoundReason`]'s arity, pinned exhaustively — **not** against the yaml,
/// and deliberately not.
///
/// Unlike `ValidationReason` and `ConflictReason`, this type is "**Not projected
/// onto the wire, and deliberately not**" (its own doc in `reason.rs`): no
/// `SCREAMING_SNAKE` constants, no `from_wire` / `as_wire`, and it never appears
/// on a `Problem` body. A published-string pin has no subject here, so none is
/// written.
///
/// What *is* pinned instead: arity, exhaustively. `NotFoundReason` carries no
/// `Unknown(String)` — it has no external producer, so there is nothing for an
/// `Unknown` catch-all to preserve, and `reason.rs`'s own doc argues this
/// directly. This test's match has no wildcard and no `Unknown` arm; a new
/// variant fails this *build*, in this crate, for the same reason the Error
/// Contract table's pin above does. The host-side `classify_record_error` match
/// in `usage-collector/src/domain/service.rs` *does* need a wildcard arm,
/// because that match is in a different crate — an asymmetry to pin rather than
/// to report.
#[test]
fn not_found_reason_has_exactly_three_variants_and_no_unknown_catch_all() {
    /// Exhaustive, wildcard-free, in the type's own declaring crate — see
    /// this test's doc.
    fn variant_name(reason: NotFoundReason) -> &'static str {
        match reason {
            NotFoundReason::DeclarationNotFound => "DeclarationNotFound",
            NotFoundReason::UsageRecordNotFound => "UsageRecordNotFound",
            NotFoundReason::InvalidationTargetNotFound => "InvalidationTargetNotFound",
        }
    }

    let names: Vec<&'static str> = [
        NotFoundReason::DeclarationNotFound,
        NotFoundReason::UsageRecordNotFound,
        NotFoundReason::InvalidationTargetNotFound,
    ]
    .into_iter()
    .map(variant_name)
    .collect();

    assert_eq!(
        names,
        [
            "DeclarationNotFound",
            "UsageRecordNotFound",
            "InvalidationTargetNotFound"
        ],
        "NotFoundReason must stay exactly these three variants with no Unknown catch-all; a \
         fourth variant needs an explicit arm above (and, per reason.rs's own doc, a deliberate \
         decision in every host-side matcher) before this test can even compile again"
    );
}
