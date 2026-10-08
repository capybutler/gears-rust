//! Unit tests for [`super`] — the reconciliation read's pure half.

use std::num::NonZeroU64;

use bigdecimal::BigDecimal;
use usage_collector_sdk::{
    AggregationFold, ObservedQuantity, QuantitySummary, UsageCollectorError, UsageQuantity,
};

use super::{
    GRANULARITY_TENANT_GTS_TYPE, RESERVED_GRANULARITIES, check_summary_branch, parse_granularity,
};

#[test]
fn the_served_granularity_is_admitted() {
    parse_granularity(GRANULARITY_TENANT_GTS_TYPE)
        .expect("v1 admits the (tenant, GTS type) granularity");
}

#[test]
fn each_reserved_granularity_is_refused_by_name_and_says_it_is_not_served() {
    // `dod-reconciliation-caller-scopes-reserved` and `inst-radmit-reserved`
    // both require an error STATING the granularity is not served. Leaving
    // this to the yaml's single-value enum gives a bare deserialization
    // failure that names nothing, which is why the two values are parsed
    // rather than rejected as unknown.
    for reserved in RESERVED_GRANULARITIES {
        let err =
            parse_granularity(reserved).expect_err("a reserved granularity is not served in v1");
        let rendered = format!("{err}");
        assert!(
            rendered.contains(reserved),
            "the rejection for `{reserved}` must name it; got: {rendered}"
        );
        assert!(
            rendered.contains("not served"),
            "the rejection for `{reserved}` must say it is not served rather than that it \
             is unknown — the granularity is reserved, and a caller told `unknown` will \
             think they misspelled it; got: {rendered}"
        );
    }
}

#[test]
fn an_unknown_granularity_is_refused_without_claiming_it_is_reserved() {
    // The two rejections share one error variant, so this asserts the DETAIL
    // (global constraints): a variant assertion cannot tell this guard from
    // the reserved-granularity guard above.
    let err = parse_granularity("per_resource").expect_err("only three spellings are known");
    let rendered = format!("{err}");
    assert!(rendered.contains("per_resource"), "got: {rendered}");
    assert!(
        !rendered.contains("reserved"),
        "an unknown granularity is a caller mistake, not a deferred feature; calling it \
         reserved promises it is coming. got: {rendered}"
    );
}

#[test]
fn the_accrued_branch_is_accepted_under_sum_and_refused_under_every_other_fold() {
    let accrued = QuantitySummary::Accrued(BigDecimal::from(1));
    check_summary_branch(AggregationFold::Sum, &accrued)
        .expect("a SUM meter reports an accrued sum");

    for fold in [
        AggregationFold::Count,
        AggregationFold::Max,
        AggregationFold::Min,
        AggregationFold::Latest,
    ] {
        let err = check_summary_branch(fold, &accrued).expect_err(
            "a non-accruing meter answered with an accrued sum is a host-contract breach",
        );
        let rendered = format!("{err}");
        assert!(
            rendered.contains("Accrued") || rendered.contains("accrued"),
            "the breach must name the branch that was returned; got: {rendered}"
        );
        assert!(
            rendered.contains(&format!("{fold:?}")),
            "the breach must name the declared fold that was expected; got: {rendered}"
        );
    }
}

#[test]
fn the_observation_branch_is_refused_under_sum() {
    let observations = QuantitySummary::Observations(None);
    let err = check_summary_branch(AggregationFold::Sum, &observations)
        .expect_err("a SUM meter answered with an observation count is a host-contract breach");
    let rendered = format!("{err}");
    assert!(rendered.contains("Sum"), "got: {rendered}");
    // Same bar as the sibling test above: `check_summary_branch` is the only
    // thing standing between a plugin returning the wrong branch and a
    // wrong figure reaching a billing surface, so a caller reading this
    // breach needs the returned branch named, not just the expected fold.
    assert!(
        rendered.contains("Observations") || rendered.contains("observation"),
        "the breach must name the branch that was returned; got: {rendered}"
    );
}

#[test]
fn the_observation_branch_is_accepted_under_every_non_sum_fold() {
    // The reject side (above) exercises all five illegal fold/branch
    // combinations; this closes the matching accept side, which the
    // original suite left to inspection alone for every fold but SUM.
    let observations = QuantitySummary::Observations(Some(ObservedQuantity {
        count: NonZeroU64::new(3).unwrap(),
        latest: UsageQuantity::parse("1").unwrap(),
    }));
    for fold in [
        AggregationFold::Count,
        AggregationFold::Max,
        AggregationFold::Min,
        AggregationFold::Latest,
    ] {
        check_summary_branch(fold, &observations)
            .expect("a non-accruing meter reports an observation count and latest");
    }
}

#[test]
fn a_branch_breach_is_internal_rather_than_a_caller_error() {
    // The caller supplied nothing that could cause this: the fold comes from
    // the declaration and the branch from the plugin. Reporting it as a 400
    // would tell an operator to fix their request.
    let err = check_summary_branch(AggregationFold::Sum, &QuantitySummary::Observations(None))
        .expect_err("expected a breach");
    assert!(
        matches!(err, UsageCollectorError::Internal { .. }),
        "got {err:?}"
    );
}

// ── The no-stall-verdict pin (spec §11.1 pin 1) ─────────────────────────

/// One thing the no-stall-verdict pin below inspects: a labelled block of
/// source text. `service.rs` cannot be checked as a whole file — every
/// dispatched operation legitimately times itself for
/// `uc_query_requests_total`, `get_reconciliation_metadata` included — so its
/// arm, when present at all, carries only that method's sliced body, never
/// the file's full text; every other source's `code` is its whole file.
///
/// Modeled on `domain::feed_tests`'s `no_feed_path_source_computes_an_age`
/// pin and its `FeedSource` / `collect_feed_sources` /
/// `slice_function_body`, reimplemented here rather than imported because
/// those helpers are private to `feed_tests`.
struct ReconciliationSource {
    label: String,
    code: String,
}

/// Recursively collect every `.rs` file under `dir` whose **file name**
/// (not its full path) contains `reconciliation` — the file name, so a
/// checkout under a directory whose own name contains `reconciliation` does
/// not pull unrelated files into the scan, mirroring
/// `domain::feed_tests::collect_rs_files_containing`'s own fix for the same
/// hazard.
fn collect_rs_files_containing(dir: &std::path::Path, out: &mut Vec<ReconciliationSource>) {
    // A read failure here must be loud, naming the path and the `io::Error`
    // — never an omission the `sources.len() >= 9` floor downstream could
    // absorb. Swallowing it silently would mean a single transient read
    // failure drops one source, the floor still passes at one below the
    // full set, and this pin quietly stops covering the file it dropped
    // while still reporting success (ruling F2, Task 7 fix round 1).
    let entries = std::fs::read_dir(dir).unwrap_or_else(|err| {
        panic!(
            "reconciliation source scan could not read directory {}: {err}",
            dir.display()
        )
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| {
            panic!(
                "reconciliation source scan could not read a directory entry under {}: {err}",
                dir.display()
            )
        });
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files_containing(&path, out);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs")
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("reconciliation"))
        {
            let code = std::fs::read_to_string(&path).unwrap_or_else(|err| {
                panic!(
                    "reconciliation source scan could not read {}: {err}",
                    path.display()
                )
            });
            out.push(ReconciliationSource {
                label: path.display().to_string(),
                code,
            });
        }
    }
}

/// Files under `api/rest/` whose **content** references a reconciliation
/// type or function, even when the file's own name does not contain
/// `reconciliation` — content-derived, scoped to the `api/rest/` subtree
/// rather than all of `src`.
///
/// **Ruling F1 (fix round 1).** The file-name derivation above cannot see
/// `api/rest/dto.rs`, which hosts `ReconciliationMetadataDto::from_metadata`
/// and `format_watermark` — the last hand-off between a domain value and a
/// REST consumer, and exactly where a convenience staleness computation
/// would land invisibly, because nothing about that file's *name* says
/// `reconciliation`.
///
/// A whole-of-`src` content scan was tried first and rejected: both
/// `domain::service_tests` and `domain::test_support` reference
/// `ReconciliationMetadata` (they define `RecordingReconciliationPlugin` and
/// drive `get_reconciliation_metadata_tests`), and both also carry
/// `OffsetDateTime::now_utc()` / `std::time::Duration::from_secs` for
/// **entirely unrelated** fixtures elsewhere in the same large,
/// multi-operation file — three such reads in `domain/service_tests.rs`
/// (two bracketing an `accepted_at` assertion, one seeding a covered
/// period) and two in `domain/test_support.rs` (`recent_window`'s shared
/// covered-period fixture and `inert_type_resolver`'s cache TTL), none of
/// them reconciliation. Reproduce with
/// `command grep -nE "OffsetDateTime::now_utc\(\)|Duration::from_secs"` over
/// either file. Scanning either
/// whole file would fail this pin on code it has no business judging — the
/// same shape of hazard `service.rs`'s own metrics timing poses, but without
/// a clean function boundary to slice around, since the reconciliation
/// mention and the unrelated clock read are just two of many things the file
/// does. `api/rest/` carries no such multi-purpose file: nothing under it
/// reads a wall clock or a duration at all (verified — see the pin's own
/// `sources.len()` evidence in the task report), so scoping the content scan
/// to that subtree reaches `dto.rs` without reopening the false-positive
/// hazard the `Reach` exemption exists to bound, not extend indefinitely.
const CONTENT_NEEDLES: &[&str] = &[
    "ReconciliationMetadataDto",
    "QuantitySummaryDto",
    "ReconciliationMetadata",
    "reconciliation::",
];

fn collect_rs_files_by_content(dir: &std::path::Path, out: &mut Vec<ReconciliationSource>) {
    // Same loud-failure discipline as `collect_rs_files_containing` above,
    // and for the identical reason: a silently skipped read must not be
    // indistinguishable from "this file has no reconciliation content".
    let entries = std::fs::read_dir(dir).unwrap_or_else(|err| {
        panic!(
            "reconciliation source scan could not read directory {}: {err}",
            dir.display()
        )
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| {
            panic!(
                "reconciliation source scan could not read a directory entry under {}: {err}",
                dir.display()
            )
        });
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files_by_content(&path, out);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            let code = std::fs::read_to_string(&path).unwrap_or_else(|err| {
                panic!(
                    "reconciliation source scan could not read {}: {err}",
                    path.display()
                )
            });
            if CONTENT_NEEDLES.iter().any(|needle| code.contains(needle)) {
                out.push(ReconciliationSource {
                    label: path.display().to_string(),
                    code,
                });
            }
        }
    }
}

/// Slice a `pub async fn <fn_name>` method's source out of an `impl` block,
/// from its signature up to whichever comes first: the next `pub async fn`
/// at the same four-space indent, or the `}` that closes the enclosing
/// `impl` block (column 0). Identical technique to
/// `domain::feed_tests::slice_function_body`.
fn slice_function_body(text: &str, fn_name: &str) -> Option<String> {
    let marker = format!("pub async fn {fn_name}(");
    let start = text.find(&marker)?;
    let rest = &text[start..];
    let tail = &rest[1..];
    let next_method = tail.find("\n    pub async fn ").map(|i| i + 1);
    let end_of_impl = tail.find("\n}\n").map(|i| i + 1);
    let end = match (next_method, end_of_impl) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    Some(match end {
        Some(end) => rest[..end].to_owned(),
        None => rest.to_owned(),
    })
}

/// The tail every [`ReconciliationSource`] built from a sliced `service.rs`
/// method carries, and the only thing that tells that arm apart from a
/// whole-file one. Mirrors `domain::feed_tests::SERVICE_ARM_LABEL_SUFFIX`.
const SERVICE_ARM_LABEL_SUFFIX: &str = "::get_reconciliation_metadata";

/// Derived, not hard-coded: walks `src/` for every reconciliation-named
/// source, adds every `api/rest/` source that references a reconciliation
/// type or function by content (ruling F1 — see
/// [`collect_rs_files_by_content`]), and adds
/// `get_reconciliation_metadata`'s body in `domain/service.rs` when that
/// method exists.
fn collect_reconciliation_sources(src_root: &std::path::Path) -> Vec<ReconciliationSource> {
    let mut sources = Vec::new();
    collect_rs_files_containing(src_root, &mut sources);
    collect_rs_files_by_content(&src_root.join("api").join("rest"), &mut sources);

    // De-duplicate: a file under `api/rest/` named `reconciliation*` (the
    // handler, its tests, the route) satisfies both derivations above, and a
    // second entry would double-count it in the floor and in every
    // assertion below — harmless, since the code is identical either way,
    // but worth collapsing to one so `sources.len()` reports distinct files.
    let mut seen_labels = std::collections::BTreeSet::new();
    sources.retain(|s| seen_labels.insert(s.label.clone()));

    // A read failure is loud here too: `service.rs` not existing
    // at all would be a real, interesting finding, not an omission this
    // function should fold into "the method arm is simply absent" — that
    // absence is what the test's own second assertion checks for, from
    // `slice_function_body` returning `None`, which is a distinct condition
    // from the file being unreadable.
    let service_path = src_root.join("domain").join("service.rs");
    let text = std::fs::read_to_string(&service_path).unwrap_or_else(|err| {
        panic!(
            "reconciliation source scan could not read {}: {err}",
            service_path.display()
        )
    });
    if let Some(body) = slice_function_body(&text, "get_reconciliation_metadata") {
        sources.push(ReconciliationSource {
            label: format!("{}{SERVICE_ARM_LABEL_SUFFIX}", service_path.display()),
            code: body,
        });
    }
    sources
}

/// Strip `//`-to-end-of-line and `/* */` comments from `text`. Identical to
/// `domain::feed_tests::strip_comments`.
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                out.push(c);
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if c == '*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            if c == '\n' {
                out.push(c);
            }
            i += 1;
            continue;
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'/') {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Remove this scan's own needle-list constant from `code` before matching.
/// Used only against this file's own text, for the same reason
/// `domain::feed_tests::redact_forbidden_needle_list` exists against
/// `feed_tests.rs`'s: this file's own name contains `reconciliation`, so the
/// naive scan finds this array literal inside itself. Deliberately does not
/// spell the declaration out in this docstring, for the same reason
/// `feed_tests`'s sibling gives: an earlier version that did got matched by
/// its own search before it ever reached the real declaration.
fn redact_forbidden_needle_list(code: &str) -> String {
    const DECL_START: &str = "const FORBIDDEN: &[(&str, Reach)] = &[\n";
    let Some(start) = code.find(DECL_START) else {
        return code.to_owned();
    };
    let Some(rel_end) = code[start..].find("];") else {
        return code.to_owned();
    };
    let end = start + rel_end + 2;
    format!("{}{}", &code[..start], &code[end..])
}

/// How widely one forbidden needle applies.
///
/// `Instant::now` and `elapsed()` are the metrics-timing pair every
/// dispatching `Service` method uses for `uc_query_requests_total`
/// (DESIGN §3.11.5) — `get_reconciliation_metadata` included. Forbidding
/// that spelling inside the sliced service arm would forbid instrumenting
/// reconciliation at all, so those two needles exempt only that one sliced
/// arm, mirroring the intent behind
/// `domain::feed_tests::Reach::FeedModulesOnly` (though not, since ruling F1,
/// its name: see [`Reach::ExceptServiceArm`]'s own doc for why). Every other
/// needle can express a stall verdict — a wall-clock read, a cadence or
/// sampling-interval read, or a named threshold/horizon quantity — and none
/// of those has any legitimate reason to appear anywhere on this path, the
/// service arm included.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Every scanned source, the `service.rs` method arm included.
    EverySource,
    /// **Every scanned source except the `service.rs` method arm** — not
    /// "the reconciliation-named modules only", despite the name this
    /// variant carried before ruling F9 (slice 5 whole-slice review). That
    /// name was accurate when this enum was written, but the `api/rest/`
    /// content scan has since added `dto.rs`, `handlers/mod.rs`,
    /// `routes/mod.rs` and `routes/registration_tests.rs` to the scanned
    /// sources this variant exempts, and none of those four is
    /// reconciliation-named. The actual rule the code enforces (see the
    /// `is_service_arm` check below) has always been "everywhere but the
    /// sliced service arm"; only the variant's name was out of date.
    ExceptServiceArm,
}

/// Fails if any reconciliation-path source grows a threshold comparison, a
/// cadence / sampling-interval read, an elapsed computation, or a timer.
///
/// `cpt-cf-usage-collector-dod-reconciliation-no-stall-verdict`:
/// `Service::get_reconciliation_metadata`'s own doc says "this path evaluates
/// nothing it returns. No threshold, no cadence read, no stalled-emitter
/// signal" — comparing a watermark against an expected cadence is the
/// consumer's work. An absence nothing can fail on is how this programme's
/// own "bounded band" survived two slices before caught it (ruling
/// E3, one layer over), so the absence is pinned rather than asserted in
/// prose.
#[test]
fn no_reconciliation_path_source_computes_a_stall_verdict() {
    const FORBIDDEN: &[(&str, Reach)] = &[
        ("OffsetDateTime::now", Reach::EverySource),
        ("SystemTime::now", Reach::EverySource),
        ("Instant::now", Reach::ExceptServiceArm),
        ("elapsed()", Reach::ExceptServiceArm),
        ("threshold", Reach::EverySource),
        ("cadence", Reach::EverySource),
        // The declared nominal sampling interval `types-registry` serves
        // (`usage-collector-v1.yaml`'s own reconciliation description) — a
        // read of it anywhere on this path is exactly the stall-evaluation
        // this pin forbids.
        ("nominal_sampling_interval", Reach::EverySource),
        ("Duration::from_secs", Reach::EverySource),
    ];

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // Derived, not hard-coded: a task adding a new reconciliation-named
    // file, or a new `api/rest/` file referencing a reconciliation type or
    // function, must not silently fall out of this check's
    // coverage. Floor is 9 *files*: the five reconciliation-named files plus
    // the four `api/rest/` files the content scan adds (`dto.rs`,
    // `handlers/mod.rs`, `routes/mod.rs`, `routes/registration_tests.rs`),
    // after de-duplication.
    //
    // `sources.len()` also counts the
    // `service.rs` method arm `collect_reconciliation_sources` appends below
    // the files, so the true count at the time of writing is 10, not 9 — a
    // `sources.len() >= 9` floor had a whole file of slack in it, and any
    // single silently-dropped file would still clear it undetected. Filtering
    // the arm out before comparing to the floor is the more honest pin: it
    // asserts exactly what the comment above claims — nine *files* — so a
    // dropped file fails here regardless of whether the arm is present,
    // rather than relying on the arm's own presence assertion below (which
    // says nothing about which of the nine files went missing).
    let sources = collect_reconciliation_sources(&root.join("src"));
    let file_count = sources
        .iter()
        .filter(|s| !s.label.ends_with(SERVICE_ARM_LABEL_SUFFIX))
        .count();
    assert!(
        file_count >= 9,
        "expected at least the nine reconciliation-relevant files (five \
         reconciliation-named, four content-matched under api/rest/); found {:?}",
        sources.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    // The derived floor above cannot see the one arm that is a *method*
    // rather than a file — see `collect_feed_sources`'s twin comment in
    // `feed_tests.rs` for why this is asserted rather than left to the
    // count alone: a slice that silently contributes nothing from
    // `service.rs` would otherwise pass with a pin that holds nothing about
    // the file most likely to acquire a stall verdict by accident.
    assert!(
        sources
            .iter()
            .any(|s| s.label.ends_with(SERVICE_ARM_LABEL_SUFFIX)),
        "the Service::get_reconciliation_metadata arm must be among the scanned \
         sources; `slice_function_body` found no `pub async fn \
         get_reconciliation_metadata(` in domain/service.rs. Found {:?}",
        sources.iter().map(|s| &s.label).collect::<Vec<_>>()
    );

    for source in &sources {
        // This test's own `FORBIDDEN` list is, unavoidably, a source of
        // every substring it forbids, since this file's own name contains
        // `reconciliation`. Only that one declaration is redacted before
        // matching; everything else in this file stays covered.
        let raw = if source.label.ends_with("reconciliation_tests.rs") {
            redact_forbidden_needle_list(&source.code)
        } else {
            source.code.clone()
        };
        // Strip comments before matching: this test's own doc discusses
        // stalls, cadence and thresholds at length, and it is the CODE that
        // must not evaluate any of them.
        let code = strip_comments(&raw);
        let is_service_arm = source.label.ends_with(SERVICE_ARM_LABEL_SUFFIX);
        for (needle, reach) in FORBIDDEN {
            if is_service_arm && *reach == Reach::ExceptServiceArm {
                continue;
            }
            assert!(
                !code.contains(needle),
                "{} contains `{needle}`; the reconciliation read evaluates nothing it \
                 returns (`cpt-cf-usage-collector-dod-reconciliation-no-stall-verdict`). \
                 If this is a false positive, narrow the pattern - do not delete the check.",
                source.label
            );
        }
    }
}
