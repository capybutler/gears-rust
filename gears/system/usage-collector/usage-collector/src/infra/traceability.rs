//! The subtree's `@cpt-*` traceability graph, read from the filesystem.
//!
//! **The documents are the oracle and the code is the subject.** This module
//! never derives an expected identifier from a marker; it reads the declared
//! sets out of `docs/` and asks whether the markers resolve against them. A
//! pin re-sourced from the code it checks is logically implied by that code
//! and therefore vacuous — ruling H51, recorded in
//! [`crate::infra::metrics_inventory`]'s own module doc.
//!
//! Scope is the usage-collector subtree: the gear crate, the SDK crate, and
//! both plugin crates, against the gear's `docs/` and each plugin's `docs/`.
//! The subtree root is `CARGO_MANIFEST_DIR/..`.
//!
//! **Corpus bounds.** The trees [`markers`] and [`documents`] walk, and no
//! others: `.rs` under each crate's and plugin's `src`, `.sql` under every
//! `plugins/*/migrations`, `.md` under the gear's and each plugin's `docs/`.
//! Directories outside the walk (`tests/`, `build.rs`, `benches/`, the
//! plugin's `README.md`, any `.yaml`) carry no `@cpt-*`-shaped lines.
//!
//! **Losing a walked tree is detected only through a changed verdict, and not
//! every tree's loss changes one.** Removing the noop plugin's `src` is
//! undetected: its markers all name one identifier that stays declared and
//! marked elsewhere, so deleting them moves no exact-equality verdict. A loss
//! is never reported *as* a read error either: [`read_files`] returns on a
//! `read_dir` failure and the walkers skip a missing `plugins/` the same way.
//!
//! **The blind spot.** This module pins that every marker points at something
//! the documents declare and that every document tick has a marker somewhere.
//! **It does not pin span coverage:** that any particular stretch of code is
//! still marked is asserted nowhere, and many marker sites can be removed with
//! every exact-equality verdict unchanged. Bulk loss is caught — deleting every
//! marker under one directory produces new unbacked identifiers — so what is
//! invisible is a deletion that leaves the identifier marked somewhere and
//! whose instance still resolves.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// One marker site: the `@cpt-` prefix, then kind, identifier, priority,
/// and an optional instance, each separated by a colon. (Spelled out away
/// from the literal `@cpt-` token so this very doc comment is not itself
/// scanned as a marker by [`markers`].)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Marker {
    pub file: String,
    pub line: usize,
    pub kind: String,
    pub identifier: String,
    pub priority: String,
    pub instance: Option<String>,
}

/// The usage-collector subtree root.
fn subtree_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| panic!("the gear crate has a parent directory"))
        .to_path_buf()
}

fn read_files(dir: &Path, exts: &[&str], out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            read_files(&path, exts, out);
            continue;
        }
        let matches = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| exts.contains(&e));
        if !matches {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        out.push((path.display().to_string(), text));
    }
}

/// The marker kinds that exist: the span brackets plus the content kinds.
/// No `featstatus` or `feature` kind exists — see [`UNBACKED_TICKS`].
const VALID_KINDS: &[&str] = &["begin", "end", "dod", "algo", "flow", "state"];

/// True if `prefix` — everything on the line before the `@cpt-` found — ends,
/// once trailing whitespace is stripped, in a comment opener (`//`, `///`,
/// `//!`, or `--`) with nothing else between the opener and `@cpt-`.
///
/// This is what tells a real marker apart from `@cpt-` sitting inside prose
/// that happens to follow a comment opener on the same line — a doc comment
/// *describing* the marker grammar, for instance. The accepted openers are
/// `//`, `//!`, and `--` (SQL migrations). The check is against what
/// immediately precedes `@cpt-` rather than the start of the line, because a
/// marker may trail a closing brace (`} // @cpt-…`).
fn comment_immediately_precedes(prefix: &str) -> bool {
    let trimmed = prefix.trim_end();
    trimmed.ends_with("//") || trimmed.ends_with("//!") || trimmed.ends_with("--")
}

/// True if `identifier` has the shape every real identifier has: `cpt-cf-`
/// followed by one or more lowercase ASCII letters, digits, or hyphens. A
/// second line of defence behind [`comment_immediately_precedes`], which is
/// the check that actually closes the self-match hole — a prose `@cpt-`
/// occurrence after a comment opener can have a valid-looking identifier.
fn is_valid_identifier(identifier: &str) -> bool {
    identifier.strip_prefix("cpt-cf-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

/// Every `@cpt-*` marker site in the subtree's source, in file order.
///
/// Counts marker *lines*, never identifier occurrences: a bare identifier in a
/// prose comment is a citation, not a marker. A line is only a marker if
/// `@cpt-` is the first non-whitespace text after a comment opener, with a kind
/// this grammar recognizes and an identifier shaped like a real one —
/// independent checks, so a loose match on any one cannot silently promote
/// prose into a marker.
#[must_use]
pub fn markers() -> Vec<Marker> {
    let root = subtree_root();
    let mut sources = Vec::new();
    for crate_dir in ["usage-collector", "usage-collector-sdk"] {
        read_files(&root.join(crate_dir).join("src"), &["rs"], &mut sources);
    }
    let plugins = root.join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins) {
        for entry in entries.flatten() {
            read_files(&entry.path().join("src"), &["rs"], &mut sources);
            read_files(&entry.path().join("migrations"), &["sql"], &mut sources);
        }
    }

    let mut out = Vec::new();
    for (file, text) in sources {
        for (idx, line) in text.lines().enumerate() {
            let Some(at) = line.find("@cpt-") else {
                continue;
            };
            if !comment_immediately_precedes(&line[..at]) {
                continue;
            }
            let rest = &line[at + "@cpt-".len()..];
            let mut fields = rest.split(':');
            let Some(kind) = fields.next() else { continue };
            if !VALID_KINDS.contains(&kind) {
                continue;
            }
            let Some(identifier) = fields.next() else {
                continue;
            };
            if !is_valid_identifier(identifier) {
                continue;
            }
            let Some(priority) = fields.next() else {
                continue;
            };
            let instance = fields
                .next()
                .map(|s| {
                    s.trim()
                        .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                        .to_owned()
                })
                .filter(|s| !s.is_empty());
            out.push(Marker {
                file: file.clone(),
                line: idx + 1,
                kind: kind.to_owned(),
                identifier: identifier.to_owned(),
                priority: priority
                    .trim()
                    .trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                    .to_owned(),
                instance,
            });
        }
    }
    out
}

/// Every document in the subtree, as `(path, text)`.
fn documents() -> Vec<(String, String)> {
    let root = subtree_root();
    let mut out = Vec::new();
    read_files(&root.join("docs"), &["md"], &mut out);
    let plugins = root.join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins) {
        for entry in entries.flatten() {
            read_files(&entry.path().join("docs"), &["md"], &mut out);
        }
    }
    out
}

fn scan_tokens(text: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '.' && c != '_')
            .unwrap_or(tail.len());
        out.push(tail[..end].to_owned());
        rest = &tail[end.max(1)..];
    }
    out
}

/// Every `cpt-cf-*` identifier any subtree document names, anywhere.
///
/// Deliberately wide: a marker resolving against a prose citation is still a
/// marker naming something the documents know about, which is what this set
/// is for. The narrower question — is it a *checkbox-bearing* identifier —
/// is [`ticked_identifiers`]'s and the per-task tick work's.
#[must_use]
pub fn declared_identifiers() -> BTreeSet<String> {
    documents()
        .iter()
        .flat_map(|(_, text)| scan_tokens(text, "cpt-cf-"))
        .collect()
}

/// Every `inst-*` step identifier any subtree document declares.
#[must_use]
pub fn declared_instances() -> BTreeSet<String> {
    documents()
        .iter()
        .flat_map(|(_, text)| scan_tokens(text, "inst-"))
        .collect()
}

/// The identifier a ticked (`- [x]`) line actually *declares*, not every
/// `cpt-cf-*` token its prose happens to cite.
///
/// A numbered flow/algo step box's own identity is its `inst-*` id — the
/// `cpt-cf-*` tokens such a line cites in prose are citations, not the subject
/// of that tick. Harvesting every token on a ticked line conflates the two: a
/// step box ticked for reasons unrelated to some algo it names in passing
/// manufactures a false tick for that algo, and once an identifier shows up
/// unbacked for that reason it reads exactly like a row that needs a real
/// excuse, and one gets written for it. So this reads only a declaration's own
/// `**ID**:` or a rollup line's sole leading identifier.
#[must_use]
pub fn ticked_identifiers() -> BTreeSet<String> {
    const ID_MARKER: &str = "**ID**:";
    let mut out = BTreeSet::new();
    for (_, text) in documents() {
        for line in text.lines() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("- [x]") && !trimmed.contains(". [x]") {
                continue;
            }
            if let Some(at) = line.find(ID_MARKER) {
                // Declaration line (`- [x] \`p1\` - **ID**: \`cpt-cf-...\``):
                // take only the identifier the marker itself declares.
                if let Some(id) = scan_tokens(&line[at + ID_MARKER.len()..], "cpt-cf-")
                    .into_iter()
                    .next()
                {
                    out.insert(id);
                }
                continue;
            }
            // A numbered flow/algo step always carries its own `inst-*` id, so
            // skipping those lines leaves the rollup shorthand
            // (`- [x] \`p1\` - \`cpt-cf-...\``): a ticked line naming exactly
            // one identifier and nothing else.
            if !line.contains("inst-") {
                let ids = scan_tokens(line, "cpt-cf-");
                if ids.len() == 1 {
                    out.extend(ids);
                }
            }
        }
    }
    out
}

/// Identifiers a marker names that no subtree document declares.
///
/// **Asserted exactly**, in both directions: a row that starts resolving fails
/// the run until it is removed, and an identifier that stops resolving without
/// a row fails too.
///
/// Each row is stated residue: the marker was checked against a candidate live
/// identifier and found not to realize it, so it stays on its retired name
/// rather than claim a correspondence the code does not hold.
pub const UNRESOLVED_IDENTIFIERS: &[(&str, &str)] = &[
    (
        "cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement",
        "Disjoint from the candidate live identifier algo-metadata-validation: \
         inst-algo-metadata-read-cap brackets the module-level \
         DEFAULT_METADATA_SIZE_CAP_BYTES const, and inst-algo-metadata-observe-bytes (three \
         call sites in service.rs) brackets a telemetry call no declared step of any live \
         identifier names",
    ),
    (
        "cpt-cf-usage-collector-dod-foundation-entity-security-context",
        "Not `entity-model`: `UsageCollectorClientV1` (api.rs) is a consumer-facing async \
         trait declaring no entity of its own, while `entity-model` is DESIGN.md's \
         concrete-entity catalog, marked at the `UsageRecord` struct definition",
    ),
    (
        "cpt-cf-usage-collector-state-usage-emission-usage-record-ingestion-lifecycle",
        "No live state-machine document describes a per-record \
         validated/persisted/spi-error/rejected-validation lifecycle; the feature's only \
         declared state machine, state-dedup-identity, describes plugin-side identity \
         convergence rather than per-request outcome",
    ),
    (
        "cpt-cf-usage-collector-dod-foundation-observability-alert-integration",
        "The closest live candidate, dod-telemetry-dashboard-routing, requires integration \
         into shared platform dashboards and routed alerts; UcMetricsMeter::new only builds \
         OTel instrument structs and configures no dashboard or alert route, so binding it \
         here would claim a correspondence the function does not hold",
    ),
    (
        "cpt-cf-usage-collector-dod-foundation-observability-pdp-helper-instruments",
        "No live identifier names a PDP instrument set this broad (pdp_ready, \
         pdp_failures, pdp_duration_seconds, authz_decisions together); the only overlapping \
         live identifier, dod-readiness-signals, is narrower (the two readiness gauges alone) \
         and already marked at its own sites",
    ),
    (
        "cpt-cf-usage-collector-dod-foundation-observability-plugin-host-instruments",
        "Same reasoning as the PDP-helper-instruments row above: no live \
         identifier names this plugin instrument set as a whole, and dod-readiness-signals \
         already covers the one overlapping readiness gauge at its own site",
    ),
    (
        "cpt-cf-usage-collector-dod-usage-query-cursor-v1-toolkit-adoption",
        "The handler is a thin wrapper: PDP authorization, PDP-constraint composition and SPI \
         dispatch all happen inside Service::list_usage_records. The live identifier that owns \
         cursor minting and decoding, dod-gateway-owned-cursor, is already marked at its own \
         sites in query.rs rather than on this handler",
    ),
];

/// Span instance identifiers that no subtree document declares. Same
/// exactly-asserted discipline as [`UNRESOLVED_IDENTIFIERS`]: each row's
/// second element states why that span carries no declared step — most often
/// because it brackets telemetry, or code byte-identical to a sibling span
/// that already carries the step.
pub const UNRESOLVED_INSTANCES: &[(&str, &str)] = &[
    // Paid off by an identifier remap that carried its spans along.
    ("inst-dod-authz-deny", "Task 2"),
    ("inst-dod-fail-closed-authz", "Task 2"),
    ("inst-dod-pluggable-storage-fail", "Task 2"),
    (
        "inst-raw-inflight-increment",
        "telemetry span, realizes no declared step of flow-query-raw-ledger-page - slice 9 residue",
    ),
    (
        "inst-raw-result-rows-observe",
        "telemetry span, realizes no declared step of flow-query-raw-ledger-page - slice 9 residue",
    ),
    (
        "inst-raw-telemetry-complete",
        "telemetry span, realizes no declared step of flow-query-raw-ledger-page - slice 9 residue",
    ),
    (
        "inst-raw-attribution",
        "brackets code byte-identical to inst-raw-scope's pdp-delegate span; carries no distinct \
         step realization - slice 9 residue",
    ),
    (
        "inst-raw-plugin-catch",
        "brackets code byte-identical to inst-raw-dispatch's plugin-dispatch span; carries no \
         distinct step realization - slice 9 residue",
    ),
    (
        "inst-raw-odata-parse",
        "brackets prepare_list_query's $top-cap/order-floor/cursor-decode; the admission checks \
         (one GTS type, one time range, no numeric offset) this step names live in \
         parse_required_gts_type_id / parse_required_time_range / reject_unknown_list_params, \
         none of them under this span - slice 9 residue",
    ),
    (
        "inst-raw-missing-ctx",
        "brackets a bare Extension<SecurityContext> parameter declaration; no conditional, no \
         rejection construction, no offending-parameter naming - slice 9 residue",
    ),
    (
        "inst-raw-metadata-filter-parse",
        "brackets parse_metadata_filters, a wire-to-typed grouping of metadata.<key> params; the \
         declared-field/declared-key validation this step names runs unspanned in \
         Service::list_usage_records (reject_unpublished_filter_fields, reject_off_label_literals, \
         require_metadata_filter_keys_declared) - slice 9 residue",
    ),
    (
        "inst-raw-request-received",
        "brackets .authenticated() on the route-registration builder chain; defensible under its \
         own retired name, not under a name meaning the consumer submits a raw read - the actual \
         request parsing is handlers.rs's and local_client.rs's own inst-raw-submit spans - \
         slice 9 residue",
    ),
    (
        "inst-aggregated-inflight-increment",
        "telemetry span, realizes no declared step of flow-query-aggregated-usage - slice 9 residue",
    ),
    (
        "inst-aggregated-result-rows-observe",
        "telemetry span, realizes no declared step of flow-query-aggregated-usage - slice 9 residue",
    ),
    (
        "inst-aggregated-telemetry-complete",
        "telemetry span, realizes no declared step of flow-query-aggregated-usage - slice 9 residue",
    ),
    (
        "inst-aggregated-attribution",
        "brackets code byte-identical to inst-agg-scope's authorize_list_usage_records span; \
         carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-aggregated-plugin-catch",
        "brackets code byte-identical to inst-agg-dispatch's plugin-dispatch span; carries no \
         distinct step realization - slice 9 residue",
    ),
    (
        "inst-aggregated-plugin-catch-return",
        "brackets code byte-identical to inst-agg-dispatch's plugin-dispatch span; carries no \
         distinct step realization - slice 9 residue",
    ),
    (
        "inst-aggregated-missing-ctx",
        "brackets a bare Extension<SecurityContext> parameter declaration; no conditional, no \
         rejection construction, no offending-parameter naming - slice 9 residue",
    ),
    (
        "inst-aggregated-request-received",
        "brackets .authenticated() on the route-registration builder chain; defensible under its \
         own retired name, not under a name meaning the consumer submits an aggregated read - the \
         actual request parsing is handlers.rs's and local_client.rs's own inst-agg-submit spans - \
         slice 9 residue",
    ),
    (
        "inst-emit-batch-missing-ctx",
        "brackets a bare Extension<SecurityContext> parameter declaration; no conditional, no \
         rejection construction, no offending-parameter naming - slice 9 residue",
    ),
    (
        "inst-get-record-missing-ctx",
        "brackets a bare Extension<SecurityContext> parameter declaration; no conditional, no \
         rejection construction, no offending-parameter naming - slice 9 residue",
    ),
    (
        "inst-get-record-submit",
        "brackets .authenticated() on the route-registration builder chain; defensible under its \
         own retired name, not under a name meaning the platform developer submits a point \
         lookup - the actual dispatch is handlers.rs's own flow - slice 9 residue",
    ),
    (
        "inst-get-record-spi-fail",
        "brackets the REST handler's one call into Service::get_usage_record (dispatch plus \
         canonical error lift); that call executes the flow's own steps internally, each already \
         spanned in service.rs under its own bound instance - slice 9 residue",
    ),
    (
        "inst-emit-batch-return-200",
        "brackets code byte-identical to inst-emit-batch-return-207's status-selection and \
         response construction (including the actual HTTP return, which inst-emit-return does \
         not contain); the 200-vs-207 split is dod-batch-outcome-model's own obligation - slice 9 \
         residue",
    ),
    (
        "inst-emit-batch-return-207",
        "brackets code byte-identical to inst-emit-batch-return-200's status-selection and \
         response construction (including the actual HTTP return, which inst-emit-return does \
         not contain); the 200-vs-207 split is dod-batch-outcome-model's own obligation - slice 9 \
         residue",
    ),
    (
        "inst-emit-batch-spi-catch",
        "brackets code byte-identical to inst-emit-dispatch's plugin-dispatch call; carries no \
         distinct step realization - slice 9 residue",
    ),
    (
        "inst-emit-batch-spi-fail-mark",
        "brackets code byte-identical to inst-emit-dispatch's plugin-dispatch call; carries no \
         distinct step realization - slice 9 residue",
    ),
    (
        "inst-emit-batch-record-accepted",
        "brackets code byte-identical to inst-emit-batch-record-conflict's and \
         -record-spi-err's settle_dispatched call, nested inside inst-emit-outcome's wider loop; \
         carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-emit-batch-record-conflict",
        "brackets code byte-identical to inst-emit-batch-record-accepted's and \
         -record-spi-err's settle_dispatched call, nested inside inst-emit-outcome's wider loop; \
         carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-emit-batch-record-spi-err",
        "brackets code byte-identical to inst-emit-batch-record-accepted's and \
         -record-conflict's settle_dispatched call, nested inside inst-emit-outcome's wider loop; \
         carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-emit-batch-observe-batch-size",
        "telemetry span (submitted batch-size histogram observation), realizes no declared step \
         of flow-emit-usage-record's step list - slice 9 residue",
    ),
    (
        "inst-emit-batch-records-counter",
        "telemetry span (per-record outcome counter), realizes no declared step of \
         flow-emit-usage-record's step list - slice 9 residue",
    ),
    (
        "inst-emit-batch-request-completion-metrics",
        "telemetry span (request-level outcome counter, duration histogram, structured log), \
         realizes no declared step of flow-emit-usage-record's step list - slice 9 residue",
    ),
    (
        "inst-emit-batch-pdp-projected-deny",
        "brackets project_pdp_decisions' projection of denied/unavailable tuple groups onto \
         per-index results; batch-internal bookkeeping no declared step names separately from \
         inst-emit-authorize's own authorization call - slice 9 residue",
    ),
    (
        "inst-emit-batch-record-pdp",
        "brackets `if !pdp_allowed[index] { continue; }`, skipping an index already decided by \
         the earlier batched PDP projection; no declared step names this per-entry skip - slice \
         9 residue",
    ),
    (
        "inst-emit-batch-record-deny",
        "brackets code byte-identical to inst-emit-batch-record-pdp's PDP-denied skip; carries \
         no distinct step realization - slice 9 residue",
    ),
    (
        "inst-emit-batch-record-unknown-usage-type",
        "brackets a declaration_cache lookup consuming an already-resolved outcome; the \
         resolve() call step 5 names runs earlier, in the unspanned per-distinct-type pre-pass - \
         slice 9 residue",
    ),
    (
        "inst-emit-batch-record-semantics",
        "brackets the invalidation-target deferral (push to pending_targets for post-loop \
         resolution); no declared step of flow-emit-usage-record's FOR-EACH substeps names \
         invalidation-target verification, which is \
         algo-usage-emission-semantics-enforcement-on-ingest-v2's own territory - slice 9 residue",
    ),
    (
        "inst-emit-batch-record-semantics-invalid",
        "brackets code byte-identical to inst-emit-batch-record-semantics's invalidation-target \
         deferral; carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-emit-batch-record-metadata-closed-shape",
        "brackets validate_submit_record_metadata, called directly rather than through \
         algo-entry-structural-validation as step 6.1 names it; the metadata cap/shape check is \
         algo-usage-emission-metadata-size-cap-enforcement's own territory - slice 9 residue",
    ),
    (
        "inst-emit-batch-record-metadata",
        "brackets code byte-identical to inst-emit-batch-record-metadata-closed-shape's \
         validate_submit_record_metadata call; carries no distinct step realization - slice 9 \
         residue",
    ),
    (
        "inst-emit-batch-record-metadata-too-large",
        "brackets code byte-identical to inst-emit-batch-record-metadata-closed-shape's \
         validate_submit_record_metadata call; carries no distinct step realization - slice 9 \
         residue",
    ),
    (
        "inst-emit-batch-record-eligible",
        "brackets eligible.push after every per-record check has passed; no declared step \
         separately names marking an entry eligible for dispatch - slice 9 residue",
    ),
    (
        "inst-get-record-pdp-deny",
        "brackets code byte-identical to inst-point-scope's authorize_get_usage_record_scope \
         call; carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-algo-attrib-receive-ctx",
        "brackets a bare SecurityContext parameter declaration (REST Extension extractor or \
         domain function signature); no conditional, no rejection construction, no \
         offending-parameter naming - slice 9 residue",
    ),
    (
        "inst-algo-attrib-batch-precondition",
        "brackets dispatch_usage_record_batch's signature through the decode loop; step 3 \
         (inst-auth-ingest-preconditions) names the per-request cap this span's cap check \
         realizes, but the span itself runs through the whole decode loop as well, far wider \
         than that one check - slice 9 residue",
    ),
    (
        "inst-algo-attrib-return",
        "lifts one per-record outcome (any kind - accepted, authorization-denied, or any other \
         rejection) to its wire DTO; not specific to the authorization gate's own outcome, and \
         the per-entry 'in input order' RETURN step 5 names is realized by the sort/collect \
         bound as inst-auth-ingest-return (service.rs), not here - slice 9 residue",
    ),
    (
        "inst-algo-attrib-compose-tuple",
        "brackets code byte-identical to inst-pdp-inputs's AccessRequest composition (building \
         the PEP request from the attribution tuple key); carries no distinct step realization - \
         slice 9 residue",
    ),
    (
        "inst-algo-pdp-compose",
        "brackets code byte-identical to inst-pdp-inputs's AccessRequest composition (building \
         the PEP request from the attribution tuple key); carries no distinct step realization - \
         slice 9 residue",
    ),
    (
        "inst-algo-pdp-call",
        "brackets code byte-identical to inst-pdp-helper's call into pdp_scope_with; carries no \
         distinct step realization - slice 9 residue",
    ),
    (
        "inst-algo-attrib-pdp-deny",
        "brackets code byte-identical to inst-pdp-helper's call into pdp_scope_with, including \
         the post-permit gate that applies algo-write-scope-admission; carries no distinct step \
         realization - slice 9 residue",
    ),
    (
        "inst-algo-attrib-pdp-allow",
        "brackets code byte-identical to inst-pdp-helper's call into pdp_scope_with, including \
         the post-permit gate that applies algo-write-scope-admission; carries no distinct step \
         realization - slice 9 residue",
    ),
    (
        "inst-auth-ingest-allow",
        "brackets project_pdp_decisions' permit branch, which compiles an invalidation-target \
         lookup scope only when the entry is a withdrawal; it does not hand the entry to domain \
         validation as step 4.5.1 (inst-auth-ingest-handoff) names - slice 9 residue",
    ),
    (
        "inst-algo-pdp-ready-gauge",
        "sets the PDP-readiness gauge (per-call inside pdp_scope_with, or once at bootstrap); \
         realizes no declared step of algo-pdp-scope-evaluation's step list - slice 9 residue",
    ),
    (
        "inst-algo-pdp-decision-metrics",
        "brackets both the gated-Ok branch and the Denied/CompileFailed branch; conflates \
         algo-pdp-scope-evaluation's step 3.1 (obtain a permit or a deny) and step 5.1 (reject \
         on a deny) under one span rather than realizing either individually - slice 9 residue",
    ),
    (
        "inst-constraint-composition-iterate",
        "brackets code byte-identical to inst-readcomp-base's scope_to_odata_filter call; \
         carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-algo-pdp-deny",
        "brackets code byte-identical to inst-failmap-deny's Denied arm; carries no distinct \
         step realization - slice 9 residue",
    ),
    (
        "inst-pdp-fail-closed",
        "brackets code byte-identical to inst-failmap-unavailable's EvaluationFailed arm; \
         carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-algo-pdp-catch",
        "brackets code byte-identical to inst-failmap-unavailable's EvaluationFailed arm; \
         carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-algo-pdp-fail-closed",
        "brackets code byte-identical to inst-failmap-unavailable's EvaluationFailed arm; \
         carries no distinct step realization - slice 9 residue",
    ),
    (
        "inst-algo-binding-cold-path",
        "wraps the same get_or_init call as inst-algo-binding-enter-selector; OnceCell branches \
         cache-hit from selector-query invisibly, so neither step is distinctly bracketable here \
         - slice 9 residue",
    ),
    (
        "inst-algo-binding-enter-selector",
        "wraps the whole get_or_init call; no single step of algo-plugin-binding-resolution is \
         distinctly bracketable at this grain - slice 9 residue",
    ),
    (
        "inst-algo-binding-readiness-fact",
        "brackets the uc_plugin_ready gauge write; no declared step of \
         algo-plugin-binding-resolution names a readiness gauge - slice 9 residue",
    ),
    (
        "inst-binding-readiness-fact",
        "brackets the uc_plugin_ready gauge write; no declared step of \
         flow-plugin-vendor-selection names a readiness gauge - slice 9 residue",
    ),
    (
        "inst-algo-plugin-dispatch-catch",
        "records the plugin-call and accept-error metrics on an Err outcome; does not classify \
         the error via algo-plugin-error-classification, which runs elsewhere - slice 9 residue",
    ),
    (
        "inst-algo-plugin-dispatch-duration",
        "records uc_plugin_call_duration_seconds on success; realizes no declared step of \
         algo-plugin-dispatch - slice 9 residue",
    ),
    (
        "inst-algo-plugin-dispatch-error-counter",
        "records uc_plugin_accept_errors_total on a backend-classified fault; realizes no \
         declared step of algo-plugin-dispatch - slice 9 residue",
    ),
    (
        "inst-algo-plugin-dispatch-error-duration",
        "records uc_plugin_call_duration_seconds on an error outcome; realizes no declared step \
         of algo-plugin-dispatch - slice 9 residue",
    ),
    (
        "inst-algo-plugin-dispatch-unready",
        "brackets resolve_plugin_for's Err arm, already covered by the outer inst-dispatch-resolve \
         span realizing step 1; this narrower bracket adds only the accept-error metric, no \
         distinct step - slice 9 residue",
    ),
    (
        "inst-algo-plugin-dispatch-unready-counter",
        "records uc_plugin_accept_errors_total{error_category=unready}; realizes no declared step \
         of algo-plugin-dispatch - slice 9 residue",
    ),
    (
        "inst-binding-meter-bootstrap",
        "brackets UcMetricsMeter::new / build_default_adapter / the init-time metrics wiring; \
         builds the whole instrument set, not a step of flow-plugin-vendor-selection - slice 9 \
         residue",
    ),
    (
        "inst-binding-return-handle",
        "brackets the ClientHub scoped-lookup-miss return; no declared step of \
         flow-plugin-vendor-selection names a ClientHub lookup miss specifically - slice 9 \
         residue",
    ),
    (
        "inst-binding-try-get-scoped",
        "brackets ClientHub::try_get_scoped; the per-call scoped lookup is \
         algo-plugin-binding-resolution's own step 6 (inst-bind-scoped-lookup), not named as a \
         flow-plugin-vendor-selection step - slice 9 residue",
    ),
    (
        "inst-algo-metadata-read-cap",
        "brackets the module-level DEFAULT_METADATA_SIZE_CAP_BYTES const (validation.rs:56), \
         35 lines above validate_submit_record_metadata (validation.rs:91); disjoint from that \
         function's body and from any \
         declared step of algo-metadata-validation, whose own Input line treats the cap as a \
         caller-supplied parameter rather than naming where its default comes from - slice 9 \
         residue",
    ),
    (
        "inst-algo-metadata-observe-bytes",
        "brackets two call sites (service.rs) of a histogram observation for \
         uc_record_metadata_bytes; telemetry, not validation - no declared step of \
         algo-metadata-validation or any other live identifier names observing this metric - \
         slice 9 residue",
    ),
    (
        "inst-algo-semantics-l1-dedup",
        "brackets resolve_invalidation_targets' distinct-lookup dedup map together with the \
         bounded fan-out and the per-entry consultation loop as one span; no single step of \
         algo-target-resolution names deduplicating repeated lookups across a batch - slice 9 \
         residue",
    ),
    (
        "inst-algo-semantics-l1-bounded-fanout",
        "brackets code byte-identical to inst-algo-semantics-l1-dedup's span; no declared step \
         of algo-target-resolution names bounding the concurrent fan-out of distinct lookups - \
         slice 9 residue",
    ),
    (
        "inst-algo-semantics-l1-lookup",
        "brackets the whole per-entry consultation loop of the batch pre-pass; realizes \
         algo-target-resolution's steps 3-7 in aggregate rather \
         than any one step individually - slice 9 residue",
    ),
    (
        "inst-algo-semantics-l1-not-found",
        "brackets the not-found rejection arm nested inside inst-algo-semantics-l1-lookup's \
         span; algo-target-resolution's own step 5 (not-found) is the closest match, but the \
         bracket here is the per-entry rejection construction, not the plugin lookup step \
         itself - slice 9 residue",
    ),
    // Governing identifier resolves; nothing rebinds these steps.
    ("inst-feed-authorize", "residue - no declared step"),
    ("inst-feed-dispatch", "residue - no declared step"),
    (
        "inst-feed-handler-compile-oldest",
        "residue - no declared step",
    ),
    ("inst-feed-handler-decode", "residue - no declared step"),
    ("inst-feed-handler-dispatch", "residue - no declared step"),
    ("inst-feed-handler-return", "residue - no declared step"),
    ("inst-feed-map-result", "residue - no declared step"),
    (
        "inst-register-route-read-feed",
        "residue - no declared step",
    ),
];

/// Identifiers ticked in a document with no marker anywhere backing them.
///
/// Slice 9 does **not** empty this one: most rows are pre-E18 ticks owned by
/// closed slices, and the rollup/featstatus rows are unmarkable by
/// construction. It is asserted exactly so the population cannot grow
/// silently, which is the regression a deletion-based remap would cause.
pub const UNBACKED_TICKS: &[(&str, &str)] = &[
    // Rollup/featstatus identifiers: unmarkable by construction, no
    // `featstatus` or `feature` marker kind exists.
    (
        "cpt-cf-uc-plugin-featstatus-per-type-retention-implemented",
        "rollup - unmarkable by construction",
    ),
    (
        "cpt-cf-uc-plugin-featstatus-reconciliation-metadata-implemented",
        "rollup - unmarkable by construction",
    ),
    (
        "cpt-cf-uc-plugin-feature-per-type-retention",
        "rollup - unmarkable by construction",
    ),
    (
        "cpt-cf-uc-plugin-feature-reconciliation-metadata",
        "rollup - unmarkable by construction",
    ),
    (
        "cpt-cf-usage-collector-featstatus-rate-limiting-reconciliation-implemented",
        "rollup - unmarkable by construction",
    ),
    (
        "cpt-cf-usage-collector-feature-rate-limiting-reconciliation",
        "rollup - unmarkable by construction",
    ),
    // Pre-E18 ticks naming their own feature document.
    (
        "cpt-cf-uc-plugin-flow-diagnose-backend-from-metrics",
        "pre-E18 tick - docs/features/observability-metrics.md",
    ),
    (
        "cpt-cf-uc-plugin-flow-startup-provision-register",
        "pre-E18 tick - docs/features/registration-schema-provisioning.md",
    ),
    (
        "cpt-cf-usage-collector-dod-plugin-consistency-profile-published",
        "pre-E18 tick - docs/features/consistency-freshness-contract.md",
    ),
    (
        "cpt-cf-usage-collector-state-cached-declaration",
        "pre-E18 tick - docs/features/usage-type-resolution.md",
    ),
    // Pre-E18 ticks that live only in the plugin's DECOMPOSITION.md.
    (
        "cpt-cf-uc-plugin-component-migrations",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-component-record-store",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-component-retention",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-constraint-dedup-key-preservation",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-constraint-gateway-owned-cursors",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-constraint-retention",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-constraint-vendor-isolation",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-db-schema",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-dbtable-usage-feed-retention-marks",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-dbtable-usage-records",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-dbtable-usage-rollup-1h",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-dbtable-usage-type-key",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-dedup-level",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-durable-ack",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-idempotent-dedup",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-per-type-retention",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-quantity-fidelity",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-reconciliation-metadata",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-record-persistence",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-schema-provisioning",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-fr-usage-feed",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-principle-pure-persistence",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
    (
        "cpt-cf-uc-plugin-principle-spi-conformance",
        "pre-E18 DECOMPOSITION.md tick (plugin)",
    ),
];

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "traceability_tests.rs"]
mod traceability_tests;
