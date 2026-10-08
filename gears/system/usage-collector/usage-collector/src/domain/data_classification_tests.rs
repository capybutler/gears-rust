//! Gear 2.10's absence pins.
//!
//! Feature 2.10's obligations are predominantly negative — MUST NOT parse an
//! identifier, MUST NOT call an identity service, MUST NOT expose an erasure
//! workflow. Ruling D1: an absence is specified as a positive requirement
//! with a pinning test, because an unpinned absence is what let this
//! programme's own "bounded band" survive unchallenged.
//!
//! **Fix round 1.** Every pin below was defeated by a reviewer-supplied
//! mutation in the first pass: a `.split_once(` parse, a bare `fn erase(`,
//! an `iam-client` manifest line, and a transitive `cf-gears-*-directory`
//! crate none of them saw. Each fix is recorded at its pin together with the
//! mutation that now catches it (or, where a mutation still gets through,
//! the bound stated plainly rather than implied).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// True if `ident` occurs in `line` as a whole identifier — the characters
/// immediately before and after the match (if any) are not identifier
/// characters. Plain [`str::contains`] on a short alias like `v` would match
/// almost every line; this is what makes carrier-alias tracking
/// ([`carrier_aliases`]) usable instead of a false-positive flood.
fn contains_ident(line: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = line[start..].find(ident) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
        let after = abs + ident.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
        if start >= line.len() {
            break;
        }
    }
    false
}

/// True if some whole-identifier occurrence of `ident` in `line` is
/// immediately followed by `suffix` (no space between them) — the shape of
/// a method call or index made directly **on** the identifier
/// (`ident.get(`, `ident[`), as distinct from the identifier merely
/// appearing elsewhere on the same line (`cache.get(&ident)`, a lookup
/// *keyed by* the identifier — equality/membership, exactly what the
/// clause admits — not a call on it). A co-occurrence scan over a loose
/// `DECOMPOSERS` list cannot tell these two shapes apart; this can, because
/// it anchors the suffix to the identifier's own occurrence rather than to
/// the line as a whole.
fn suffix_immediately_after(line: &str, ident: &str, suffix: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = line[start..].find(ident) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
        let after = abs + ident.len();
        if before_ok && line[after..].starts_with(suffix) {
            return true;
        }
        start = abs + 1;
        if start >= line.len() {
            break;
        }
    }
    false
}

/// Rust's indexing/slicing syntax (`subject_id[..4]`, `resource_id[4..]`),
/// which carries no method-name substring and so is invisible to a
/// decomposer-vocabulary scan. A thin specialization of
/// [`suffix_immediately_after`].
fn indexed_immediately_after(line: &str, ident: &str) -> bool {
    suffix_immediately_after(line, ident, "[")
}

/// Every production `.rs` file under this crate's `src/`, as
/// `(absolute path, contents)`.
///
/// Re-implemented locally rather than imported from
/// `service_metrics_tests.rs`'s `production_sources()`: that oracle is
/// private to its module, and moving it is a refactor outside this task —
/// the same boundary `metrics_inventory.rs`'s module doc records.
///
/// Fails loudly on an unreadable file or directory, and on an **empty**
/// result. Fix round 2 widened "fails loudly" to cover a
/// *partial* shrink, not only a total one: `walk`'s own `read_dir` error and
/// every per-entry error used to be swallowed (`let Ok(entries) =
/// read_dir(dir) else { return }`, `entries.flatten()`), so one unreadable
/// subdirectory or one bad directory entry would silently drop part of the
/// corpus while the non-empty guard below kept passing — only *total*
/// emptiness fired it. Both now propagate via `panic!`, matching
/// [`service_metrics_tests::production_sources`]'s own shape exactly.
fn gear_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot read directory {}: {e}", dir.display()));
        for entry in entries {
            let entry =
                entry.unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", dir.display()));
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let name = path
                .file_name()
                .expect("a file has a name")
                .to_string_lossy()
                .into_owned();
            if name.ends_with("_tests.rs") || name == "test_support.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            out.push((path.display().to_string(), text));
        }
    }
    let mut out = Vec::new();
    walk(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut out,
    );
    assert!(
        !out.is_empty(),
        "no production source found under this crate's src/; an absence pin over an empty \
         corpus asserts nothing"
    );
    out
}

/// Sites the forbidden-vocabulary scan below hits, read and judged benign,
/// each excepted by its file suffix and its exact trimmed line text (not by
/// widening the vocabulary — doing that would make the pin vacuous).
///
/// Both rows are prose in `sdk_error_mapping.rs` describing that an
/// `Internal` error's `detail` string arrives from the SDK boundary already
/// scrubbed of connection-string-shaped content before this crate ever sees
/// it — a generic diagnostic-safety property of one opaque string field, not
/// a redaction *operation* this gear performs on a personal-data field, and
/// not reachable from any of the four identifier carriers or from metadata.
/// Verified by reading `sdk_error_mapping.rs:330-334`: the line is a comment
/// on the match arm `E::Internal { detail } => CanonicalError::internal(detail).create()`,
/// which passes `detail` through unchanged — no masking call exists on this
/// path.
///
/// **Fix round 1 added the rows below**, after widening
/// `FORBIDDEN` to `mask`/`forget`/`delete` surfaced a batch of ordinary
/// English and ordinary bookkeeping prose. Each was read at its site before
/// being added; none is a privacy-workflow operation:
///
/// - The four `mask`/`forget` hits in `handlers/usage_records.rs` and
///   `type_resolver/metadata.rs` use "mask" in the ordinary English sense
///   ("ambiguity cannot mask a caller bug" / "cannot also be masked...by
///   whatever the subschema's own keyword says") — obscuring a *different*
///   error, not applying a redaction operation to a value.
/// - The `forget` hits in `type_resolver/mod.rs`, `service.rs`,
///   `observability.rs` and `query.rs` are the type-resolution cache's own
///   `forget_gate` eviction naming, the `fire-and-forget` concurrency idiom,
///   and two instances of plain English ("a caller could forget," "no
///   implementor to forget") — none is about forgetting a *person*.
/// - Every `delete` hit is either an HTTP-verb audit
///   (`routes/usage_records.rs`'s `grep ... put\|patch\|delete`, confirming
///   **no** such verb is registered — the opposite of a violation), the
///   declaration-mirror cache's own retirement language, a dimension/metric
///   bookkeeping note, or this crate's own `traceability.rs`/
///   `metrics_inventory.rs` module docs describing *their* const-list
///   housekeeping (a stale row "deleted," a duplicate marker "kept rather
///   than deleted") — never a persisted entry or identifier.
///
/// **Fix round 2 added two more**, surfaced by widening `forget` to also
/// catch `forgot`/`forgotten`: `service.rs`'s "not an edge case it forgot"
/// (ordinary English about a design gap) and `type_resolver/mod.rs`'s
/// "restores a forgotten one from that table" (the same type-declaration
/// cache-eviction language the round 1 rows already cover, talking about a
/// GTS type declaration, not a person).
///
/// **Asserted in both directions** (added at closeout, after a reviewer
/// showed a bogus row leaves all six data-classification pins green):
/// every row must still be matched by a scanned file *and* must still name
/// a `FORBIDDEN` token, or
/// [`the_gear_exposes_no_privacy_workflow_operation`] fails until it is
/// deleted. The risk here is rot rather than a false green — a row matches
/// an exact trimmed line plus a file-name suffix, so a stale one is inert
/// — but inert rows accumulate and a later line that happens to match one
/// would be waved through with no reader ever having looked at it.
const PRIVACY_WORKFLOW_EXCEPTIONS: &[(&str, &str)] = &[
    (
        "sdk_error_mapping.rs",
        "/// assert in debug, surface a redacted `internal` on the wire rather than",
    ),
    (
        "sdk_error_mapping.rs",
        "// `detail` is DSN-free and pre-redacted at the construction site by",
    ),
    // An `authz.rs` row stood here from fix round 1 until closeout,
    // excusing the line `"/// eight's forbidden display-obscuring pair), 9
    // (no normalize beyond the"`. The both-directions assertion added
    // below found it on its first run: `git grep` for that text returns
    // only the row itself, and `git log -S` over the whole repository
    // returns only the commit that *added the row*. The line it names has
    // never existed in any file. It was inert from the day it was written
    // and nothing noticed across five commits — which is the case for
    // asserting this list both ways rather than only as a filter.
    (
        "handlers/usage_records.rs",
        "/// occurrence is rejected so silent last-wins ambiguity cannot mask a",
    ),
    (
        "handlers/usage_records.rs",
        "/// [`require_single_value`], so last-wins ambiguity cannot mask a caller",
    ),
    (
        "type_resolver/metadata.rs",
        "/// so it cannot also be masked or double-reported by whatever the",
    ),
    (
        "domain/observability.rs",
        "//! omission structural rather than a discipline a caller could forget \u{2014}",
    ),
    (
        "domain/query.rs",
        "/// \u{2014} the gear mints, never a plugin, so there is no implementor to forget",
    ),
    (
        "domain/service.rs",
        "/// is returned unchanged; metric emission is fire-and-forget and never mutates",
    ),
    (
        "type_resolver/mod.rs",
        "//! `types-registry` stores declarations in memory, so a restart forgets",
    ),
    (
        "type_resolver/mod.rs",
        "//! restores a forgotten one from that table the next time the registry",
    ),
    (
        "type_resolver/mod.rs",
        "//! `infra::declaration_mirror`'s table \u{2014} is deleted in one commit when",
    ),
    (
        "type_resolver/mod.rs",
        "/// they gate completes ([`Self::forget_gate`]), so this map tracks keys",
    ),
    ("type_resolver/mod.rs", "self.forget_gate(id).await;"),
    (
        "type_resolver/mod.rs",
        "async fn forget_gate(&self, id: &MeterTypeId) {",
    ),
    (
        "routes/usage_records.rs",
        "/// `grep -rn 'OperationBuilder::\\(put\\|patch\\|delete\\)' usage-collector/src/`",
    ),
    (
        "routes/usage_records.rs",
        "/// deletes or flags a stored entry. A withdrawn entry therefore has no",
    ),
    (
        "domain/covered_period.rs",
        "/// retention deletes was accepted at least H + S before its drop: its",
    ),
    (
        "ports/declaration_mirror.rs",
        "//! statement 7: the mirror and the restore *\"are deleted when persistent",
    ),
    (
        "ports/declaration_mirror.rs",
        "//! *\"outlives them\"*. Retiring the bridge deletes this file, its sibling",
    ),
    (
        "ports/declarations.rs",
        "/// `cpt-cf-usage-collector-adr-declaration-rehydration` statement 7 deletes",
    ),
    (
        "ports/declarations.rs",
        "/// rather than inside it, so retiring the bridge deletes this trait and its",
    ),
    (
        "ports/metrics.rs",
        "/// Replaces the deleted catalog-lifecycle instruments",
    ),
    (
        "ports/metrics.rs",
        "/// instrument that replaced the deleted usage-type catalog counters.",
    ),
    (
        "domain/query.rs",
        "/// duplicate and tell the caller to delete one when what they have is a",
    ),
    (
        "domain/query.rs",
        "// second would tell them to delete a duplicate when what they have is a",
    ),
    (
        "declaration_mirror/entity.rs",
        "//! `cpt-cf-usage-collector-adr-declaration-rehydration` statement 7 deletes",
    ),
    (
        "infra/metrics.rs",
        "/// Type Resolver instrument that replaced the deleted usage-type catalog",
    ),
];

/// `cpt-cf-usage-collector-dod-no-gear-local-privacy-workflow`: no consent,
/// data-subject-request, erasure, purge, anonymization or masking operation
/// on any surface.
///
/// Scans production source for the operation vocabulary. A hit is not
/// automatically a violation — it is a site a human must read — which is why
/// the assertion names the file and line rather than only a count, and
/// [`PRIVACY_WORKFLOW_EXCEPTIONS`] excepts specific lines rather than
/// weakening `FORBIDDEN`.
///
/// **Fix round 1.** The first-pass list was never wide enough to bite:
/// `erase_` excluded the single likeliest spelling (`fn erase(`), `mask_`
/// excluded `mask(`/`masking`/`masked`, and `data_subject` excluded the
/// hyphenated route-literal spelling a real DSR endpoint would carry
/// (`data-subject-requests`). Widened to match pin 4's wording (which was
/// already wider on `erase`/`mask`/`delete` and never back-ported here) plus
/// the words a real erasure surface's own vocabulary would use:
/// `forget`/`scrub`/`wipe`/`obfuscat`/`pseudonym`/`sanitiz`/`gdpr`/
/// `tombstone`.
///
/// **The bare abbreviation `dsr` was tried and dropped.** As a plain
/// substring it collides with this gear's own `CreateUsageRecordsRequest` /
/// `CreateUsageRecordsResponse` type names (`...Record` + `sRequest` spells
/// `dsr` mid-identifier) — fifteen hits, all the same collision, none a
/// data-subject-request surface. `data_subject` and `data-subject` above
/// already carry the spelled-out, low-collision forms; the abbreviation
/// added no real coverage past those two for the false-positive cost.
///
/// **Fix round 2 closed two gaps the round 1 list still had.**
/// `forget` does not match `forgotten` — "forget" and "forgot" diverge at
/// the fourth letter, so the canonical GDPR phrase "the right to be
/// forgotten" (and `right_to_be_forgotten`-shaped identifiers) walked
/// straight through. Added `forgot`, which both `forget`'s own inflections
/// (`forgot`, `forgotten`) and `forget` itself (`forget` contains neither
/// `forgot` nor vice versa — both stay, now covering the family between
/// them) are built from. And round 1 fixed the `-ize`/`-ise` gap for
/// `erase`/`mask` by using the bare root, but did not generalise the fix to
/// the two other `-ize`-rooted words already on this list — `anonymiz` does
/// not match the British `anonymise`, `sanitiz` does not match `sanitise` —
/// so `anonymis`/`sanitis` are added beside them, and a word that was
/// entirely absent either way, `tokeniz`/`tokenis` (data tokenization is the
/// same class of operation as masking — replacing a value with a
/// non-reversible or vaulted substitute), plus `pii` for the bare
/// abbreviation (`strip_pii`-shaped names carry no other word on this list).
#[test]
fn the_gear_exposes_no_privacy_workflow_operation() {
    const FORBIDDEN: &[&str] = &[
        "consent",
        "data_subject",
        "data-subject",
        "erasure",
        "erase",
        "purge",
        "anonymiz",
        "anonymis",
        "redact",
        "mask",
        "delete",
        "forget",
        "forgot",
        "scrub",
        "wipe",
        "obfuscat",
        "pseudonym",
        "sanitiz",
        "sanitis",
        "tokeniz",
        "tokenis",
        "pii",
        "gdpr",
        "tombstone",
    ];
    let sources = gear_sources();
    let hits: BTreeSet<String> = sources
        .iter()
        .flat_map(|(file, text)| {
            text.lines().enumerate().filter_map(move |(i, line)| {
                let lower = line.to_lowercase();
                let trimmed = line.trim();
                let excepted = PRIVACY_WORKFLOW_EXCEPTIONS
                    .iter()
                    .any(|(suffix, excepted_line)| {
                        file.ends_with(suffix) && *excepted_line == trimmed
                    });
                if excepted {
                    return None;
                }
                FORBIDDEN
                    .iter()
                    .find(|t| lower.contains(**t))
                    .map(|t| format!("{file}:{} names `{t}`: {}", i + 1, trimmed))
            })
        })
        .collect();

    assert!(
        hits.is_empty(),
        "feature 2.10 forbids a gear-local privacy workflow; read each site \
         and either remove it or widen this pin's stated exceptions: {hits:#?}"
    );

    // The other direction. Without this, the list is a one-way filter:
    // a reviewer added `("nowhere.rs", "// a line that exists in no source
    // file at all")` and all six data-classification pins stayed green.
    // `UNRESOLVED_IDENTIFIERS` / `UNRESOLVED_INSTANCES` / `UNBACKED_TICKS`
    // have asserted both directions since task 1 precisely so that a row
    // which stops being needed reds the run instead of rotting in place;
    // this brings these exceptions to the same discipline. A row is live
    // only if some scanned file whose path ends with its suffix carries
    // that exact trimmed line **and** that line would otherwise be a hit.
    let inert: Vec<String> = PRIVACY_WORKFLOW_EXCEPTIONS
        .iter()
        .filter(|(suffix, excepted_line)| {
            let lower = excepted_line.to_lowercase();
            let would_hit = FORBIDDEN.iter().any(|t| lower.contains(t));
            let present = sources.iter().any(|(file, text)| {
                file.ends_with(suffix) && text.lines().any(|line| line.trim() == *excepted_line)
            });
            !(would_hit && present)
        })
        .map(|(suffix, excepted_line)| format!("{suffix}: {excepted_line}"))
        .collect();

    assert!(
        inert.is_empty(),
        "these PRIVACY_WORKFLOW_EXCEPTIONS rows except nothing: either no scanned file ending \
         with that suffix still carries the line verbatim, or the line no longer names any \
         FORBIDDEN token. A row that has stopped doing work must be deleted, not left to rot \
         into cover for a future line that happens to match it: {inert:#?}"
    );
}

/// `cpt-cf-usage-collector-dod-no-identity-enrichment`'s second sentence,
/// **direct-dependency half only** (see
/// [`the_gear_s_transitive_dependency_graph_names_no_identity_or_people_directory_crate`]
/// for the transitive half).
///
/// **Fix round 1.** This pin's own previous doc claimed "a transitive
/// identity client would not appear in any `use` line the gear wrote" as if
/// that were a reason this Cargo.toml read covered the transitive case —
/// it is not. A manifest read sees **direct** dependencies only; a sibling
/// test below now reads the actual resolved dependency graph (`cargo tree`)
/// for the transitive claim. This test's job is narrower and stated as
/// such: did whoever edited this gear's own `Cargo.toml` add a line naming
/// an identity-shaped crate.
///
/// Vocabulary widened: `account-service` only matched the
/// compound, letting `account-client` through; `iam`, `oidc`,
/// `keycloak`, `okta`, `scim`, `people`, `principal`, `subject`, `user` were
/// entirely absent. Checked against this crate's actual `Cargo.toml` text
/// (151 lines) for false positives from the widened list: none found.
#[test]
fn the_gear_declares_no_identity_directory_or_profile_dependency() {
    const FORBIDDEN: &[&str] = &[
        "identity",
        "directory",
        "ldap",
        "account",
        "iam",
        "oidc",
        "keycloak",
        "okta",
        "scim",
        "people",
        "principal",
        "subject",
        "user",
        "profile",
    ];
    let manifest =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("the gear has a manifest");

    let hits: Vec<&str> = manifest
        .lines()
        .filter(|l| {
            let lower = l.to_lowercase();
            // Fix round 1: a `[profile.release]`-shaped TOML
            // section header configures build settings for an
            // already-declared dependency; it cannot itself add one, so it
            // is not a hit on "profile" even though the clause's own word
            // appears literally. A dependency line is always `key = {...}`,
            // never a bare `[...]` header, so this guard cannot hide one.
            if lower.trim_start().starts_with("[profile") {
                return false;
            }
            FORBIDDEN.iter().any(|t| lower.contains(t))
        })
        .collect();

    assert!(
        hits.is_empty(),
        "feature 2.10 bounds the direct outbound dependency set: {hits:#?}"
    );
}

/// Transitive dependencies named here are real and verified benign, not
/// textual accidents — unlike [`PRIVACY_WORKFLOW_EXCEPTIONS`], which excepts
/// prose.
///
/// `cf-gears-system-sdk-directory` is a **service-discovery** directory — it
/// answers "which platform services exist and where," the same shape as
/// `ClientHub`'s own instance registry — not a people/identity directory.
/// Verified by reading its own crate (`libs/system-sdks/sdks/directory`):
/// its public surface is instance/service lookup, with no field, method, or
/// type shaped like a person record (no name, contact detail, or subject
/// identifier resolution). It reaches this gear transitively through
/// `cf-gears-toolkit`, not through anything this gear's own `Cargo.toml`
/// names, which is exactly why the direct-dependency pin above cannot see
/// it and this one must.
///
/// **Asserted in both directions** (added at closeout, for the same reason
/// as [`PRIVACY_WORKFLOW_EXCEPTIONS`]): every row must still appear in the
/// resolved graph `cargo tree` reports *and* must still match the forbidden
/// vocabulary, or the test below fails until it is deleted. The one
/// weakening this keeps: that assertion sits after the environment skips,
/// so a run where `cargo tree` could not resolve checks neither direction.
const TRANSITIVE_DEPENDENCY_EXCEPTIONS: &[&str] = &["cf-gears-system-sdk-directory"];

/// `cpt-cf-usage-collector-dod-no-identity-enrichment`'s second sentence,
/// **transitive half**: "Its outbound dependency set MUST stay limited to
/// the platform PDP, the type registry, and the bound storage plugin."
///
/// **Fix round 1.** Two reviewers independently found the same hole from
/// opposite directions: one, that `#[toolkit::gear(deps = …)]` is a
/// gear-to-gear linking / bootstrap-graph declaration
/// (`libs/toolkit-macros/src/lib.rs`'s `gear` macro doc) with no bearing on
/// runtime client reachability; the other, that `cargo tree -i
/// cf-gears-system-sdk-directory -p cf-gears-usage-collector` shows that
/// crate live in the normal dependency graph today, past every existing
/// pin, green. Neither a `Cargo.toml` read nor a `use`-line scan can see a
/// transitive crate — only the resolved graph can, so this test asks Cargo
/// for it directly rather than re-deriving an expected answer from this
/// gear's own source (which would be the H51 vacuity the opposite way).
///
/// **Fix round 2 hardened how that ask happens**, after the round 1 version
/// was shown to red (or slow) the whole lane for environment reasons having
/// nothing to do with this gear's dependency graph:
///
/// - `Command::new("cargo")` depended on `cargo` being on `PATH`, which is
///   not guaranteed for whatever process ends up running the compiled test
///   binary. Reading `std::env::var("CARGO")` first uses the path Cargo
///   itself already set for this very process, falling back to the bare
///   name only if that variable is absent.
/// - The round 1 call carried no `--offline`/`--locked` flags, so on a
///   cold-but-reachable cache it made an unmarked registry fetch (measured:
///   316 MB, 15.19 s, against 0.43 s warm — a cost this test's own passing
///   run never disclosed) and, with a stale lockfile, could rewrite
///   `Cargo.lock` mid test run. `--offline --locked` are passed now: no
///   fetch, and no silent rewrite.
/// - **A spawn failure or a non-zero exit is an inconclusive environment
///   problem, not a finding, and is skipped with a loud `eprintln!` rather
///   than asserted as a violation** — neither outcome has looked at a
///   single dependency name, so failing the build on either would make this
///   pin's redness mean "something about reaching Cargo went wrong" as
///   often as "a forbidden crate is in the graph," indistinguishable from
///   the pin's actual purpose. This does not relax the vacuity guard below:
///   a **successful** exit with **empty** output still panics, because a
///   quiet, well-behaved process that looked at nothing is exactly the
///   failure mode that guard exists for.
///
/// Shells out to `cargo tree -p cf-gears-usage-collector -e normal --prefix
/// none --offline --locked`, normal-dependency edges only (no dev/build
/// dependencies, which never ship), takes each output line's first
/// whitespace-separated token as a crate name, and checks it against the
/// clause's own forbidden vocabulary. [`TRANSITIVE_DEPENDENCY_EXCEPTIONS`]
/// excepts the one crate this already flags today, verified benign by
/// reading it, not by softening the vocabulary.
///
/// **This is a vocabulary denylist over the resolved graph, not the
/// three-member allowlist the clause's own words describe, and the
/// assertion message says so rather than claiming otherwise** — a
/// transitive crate named, say, `tenant-lookup` or `person-registry` would
/// pass this scan undetected. Enumerating every legitimate crate this gear
/// may transitively depend on, and keeping that enumeration current across
/// every `cargo update`, is not a test anyone could maintain; claiming this
/// scan proves the allowlist anyway was fix round 1's own copy of the
/// defect it was raised to fix in `module.rs`.
///
/// **Fix round 3 narrowed the skip.** `cargo test` always sets `CARGO` for
/// the process it spawns, so that variable being absent means this binary
/// is running outside `cargo test` entirely — a case where a silent skip
/// hides the most and costs nothing to rule out. That path now panics
/// instead of falling back to a bare `cargo` lookup. The resolution-failure
/// skip (a non-zero `cargo tree` exit) stays a skip, and its residual bound
/// is stated here rather than left implicit: **that skip leaves the test
/// green with its message captured, so a persistently unresolvable
/// workspace would silence this pin with no visible signal under a default
/// `cargo test` run.** What keeps this tolerable is that `cargo tree`
/// resolves the **whole workspace** to produce this gear's slice of it, so
/// the realistic trigger is an unrelated workspace member failing to
/// resolve offline while this gear itself builds fine — exactly the shape
/// reproduced against `cf-gears-example-server`'s `anyhow` dependency — not
/// a problem with this gear's own dependency graph going quietly
/// unexamined.
#[test]
fn the_gear_s_transitive_dependency_graph_names_no_identity_or_people_directory_crate() {
    const FORBIDDEN: &[&str] = &[
        "identity",
        "directory",
        "ldap",
        "account",
        "iam",
        "oidc",
        "keycloak",
        "okta",
        "scim",
        "people",
        "principal",
        "subject",
        "user",
        "profile",
    ];

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| {
        panic!(
            "CARGO environment variable is unset; `cargo test` always sets it for the test \
             binary it spawns, so this means the binary is running outside `cargo test` \
             entirely \u{2014} the resolution-failure skip below exists for a genuinely \
             environmental case (an unrelated workspace member failing to resolve offline), \
             not for this"
        )
    });
    let spawned = std::process::Command::new(&cargo)
        .args([
            "tree",
            "-p",
            "cf-gears-usage-collector",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--offline",
            "--locked",
        ])
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
        .output();

    let output = match spawned {
        Ok(output) => output,
        Err(e) => {
            eprintln!(
                "SKIPPED (environment, not a finding): could not spawn `{cargo} tree` ({e}); \
                 this pin did not examine the dependency graph this run"
            );
            return;
        }
    };
    if !output.status.success() {
        eprintln!(
            "SKIPPED (environment, not a finding): `{cargo} tree --offline --locked` exited \
             non-zero — the workspace did not resolve offline against the locked lockfile \
             (cold cache, unrelated member broken, or similar); this pin did not examine the \
             dependency graph this run: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);

    let names: BTreeSet<&str> = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert!(
        !names.is_empty(),
        "`cargo tree` exited successfully but produced no crate names; this pin would be vacuous"
    );

    // The other direction, for the same reason the privacy-workflow list
    // now carries one: a reviewer added `"cf-gears-nonexistent-identity-crate"`
    // to this list and all six data-classification pins stayed green. An
    // exception is live only if the crate is really in the resolved graph
    // **and** really matches the forbidden vocabulary — if either stops
    // holding, the row is excusing nothing and must go.
    let inert: Vec<&&str> = TRANSITIVE_DEPENDENCY_EXCEPTIONS
        .iter()
        .filter(|excepted| {
            let lower = excepted.to_lowercase();
            !(names.contains(*excepted) && FORBIDDEN.iter().any(|t| lower.contains(t)))
        })
        .collect();
    assert!(
        inert.is_empty(),
        "these TRANSITIVE_DEPENDENCY_EXCEPTIONS rows except nothing: the crate is no longer in \
         this gear's resolved normal-dependency graph, or it no longer matches the forbidden \
         vocabulary. Delete the row rather than leave it standing as cover for a future crate \
         of the same name: {inert:#?}"
    );

    let hits: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| {
            if TRANSITIVE_DEPENDENCY_EXCEPTIONS.contains(name) {
                return false;
            }
            let lower = name.to_lowercase();
            FORBIDDEN.iter().any(|t| lower.contains(t))
        })
        .collect();

    assert!(
        hits.is_empty(),
        "the transitive dependency graph names a crate matching feature 2.10's forbidden \
         identity/directory vocabulary (a vocabulary denylist, not the clause's three-member \
         allowlist — a differently-named identity-shaped crate would pass this scan \
         undetected): {hits:#?}"
    );
}

/// `cpt-cf-usage-collector-dod-no-gear-local-privacy-workflow`'s second
/// sentence: "The storage plugin interface MUST carry no erasure or
/// redaction method." This is the one clause of feature 2.10 that genuinely
/// reaches the SPI — `UsageCollectorPluginV1` lives in the
/// `usage-collector-sdk` crate, not this one, so [`gear_sources`] (this
/// crate's own `src/`) cannot see it.
///
/// Reads the SDK crate's trait file directly rather than walking from this
/// crate's `CARGO_MANIFEST_DIR`, and scans every `fn` declaration's own
/// name — not the whole file's prose — because the clause binds the
/// interface's **method surface**, not its doc comments.
///
/// The trait's doc comment claims the methods below are its **entire** method
/// surface. Checking vocabulary alone leaves that claim unbacked — a
/// default-bodied method added to the trait falsifies it with the pin green —
/// so the count assertion backs the word "entire": growth alone fails the pin,
/// vocabulary or not. The count lives here, in an assertion that fails loudly,
/// rather than in the prose, where it would drift in silence.
#[test]
fn the_storage_plugin_interface_declares_no_erasure_or_redaction_method() {
    const FORBIDDEN: &[&str] = &[
        "consent",
        "data_subject",
        "data-subject",
        "erasure",
        "erase",
        "purge",
        "anonymiz",
        "anonymis",
        "redact",
        "mask",
        "delete",
        "forget",
        "forgot",
        "scrub",
        "wipe",
        "obfuscat",
        "pseudonym",
        "sanitiz",
        "sanitis",
        "tokeniz",
        "tokenis",
        "pii",
        "gdpr",
        "tombstone",
    ];
    let sdk_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the gear crate has a parent directory")
        .join("usage-collector-sdk");
    let plugin_api_path = sdk_root.join("src").join("plugin_api.rs");
    let text = std::fs::read_to_string(&plugin_api_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", plugin_api_path.display()));

    let method_lines: Vec<&str> = text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("fn ") || t.starts_with("async fn ") || t.starts_with("pub fn ")
        })
        .collect();
    assert_eq!(
        method_lines.len(),
        6,
        "the trait's doc comment claims the methods below are its entire method surface; \
         method count changed: {method_lines:#?}"
    );

    let hits: Vec<&str> = method_lines
        .iter()
        .copied()
        .filter(|l| {
            let lower = l.to_lowercase();
            FORBIDDEN.iter().any(|t| lower.contains(t))
        })
        .collect();

    assert!(
        hits.is_empty(),
        "the storage plugin interface must carry no erasure or redaction method: {hits:#?}"
    );
}

/// Carrier names, widened per-file to include simple direct local aliases —
/// `let v = tenant_id;` or `let v = tenant_id.clone();` — found by a
/// fixed-point scan of `let IDENT = <carrier-or-known-alias>(.clone())?;`
/// lines, up to four hops (`let a = tenant_id; let b = a; let c = b; ...`).
/// Fix round 1: a reviewer's `let v = gts_type_id; v.split('/')` two-line
/// case passed the first version because the carrier name and the
/// decomposer call never shared a line.
///
/// **Two bounds, stated rather than implied.** This does not chase an alias
/// through a function call, a destructuring pattern, or a field projection
/// (`key.tenant_id`) — those remain genuinely open. And an alias here is
/// **file-scoped, not block-scoped**: once a short name like `v` is
/// registered anywhere in a file, every line of that *whole file* is
/// checked against it, including prose inside doc comments and unrelated
/// functions that happen to reuse the same short name for something else
/// entirely. This over-reports — a false hit still needs a human to read
/// and dismiss it, same as any other hit this module scans for — but it
/// never under-reports, which is the direction that would matter: the scan
/// cannot be made to miss a real decomposition by shadowing the alias name
/// somewhere else in the same file.
fn carrier_aliases(text: &str, carriers: &[&str]) -> Vec<String> {
    let mut aliases: Vec<String> = carriers.iter().map(|s| (*s).to_owned()).collect();
    for _ in 0..4 {
        let mut added = Vec::new();
        for line in text.lines() {
            let Some(rest) = line.trim_start().strip_prefix("let ") else {
                continue;
            };
            let Some(eq_pos) = rest.find('=') else {
                continue;
            };
            let (lhs, rhs) = rest.split_at(eq_pos);
            let lhs_ident = lhs
                .trim()
                .trim_start_matches("mut ")
                .split(':')
                .next()
                .unwrap_or("")
                .trim();
            let rhs_base = rhs[1..]
                .trim()
                .trim_end_matches(';')
                .trim()
                .trim_end_matches(".clone()")
                .trim();
            if lhs_ident.is_empty() || rhs_base.is_empty() {
                continue;
            }
            let rhs_is_known = aliases.iter().any(|a| a == rhs_base);
            let lhs_already_known =
                aliases.iter().any(|a| a == lhs_ident) || added.contains(&lhs_ident.to_owned());
            if rhs_is_known && !lhs_already_known {
                added.push(lhs_ident.to_owned());
            }
        }
        if added.is_empty() {
            break;
        }
        aliases.extend(added);
    }
    aliases
}

/// `cpt-cf-usage-collector-dod-opaque-identifier-treatment`: tenant, subject,
/// resource and GTS type identifiers are carried byte for byte.
///
/// **Fix round 1.** The first-pass `DECOMPOSERS` list missed
/// `.split_once(`/`.rsplit_once(` (the idiomatic Rust prefix/suffix parse —
/// step 2's literal target), `.to_ascii_lowercase()`/`.to_ascii_uppercase()`
/// (step 9's normalization — `.to_lowercase()` alone does not match the
/// ASCII-specific spelling), `.starts_with(`/`.ends_with(`/`.parse(`/
/// `Uuid::parse_str(` (step 2), and bracket indexing (`subject_id[..4]`,
/// step 2 again — no method-name substring exists for this one, which is
/// why [`indexed_immediately_after`] checks adjacency instead of
/// vocabulary). [`carrier_aliases`] closes the two-line alias case; what it
/// still cannot see is documented at its own definition.
///
/// **Fix round 2** added `.find(`/`.get(`/`.bytes(` the same adjacency way,
/// not via the loose `DECOMPOSERS` list: `resource_id.get(0..idx)` after
/// `resource_id.find(':')` is a real decomposition (a range slice following
/// a located separator — step 2 again), but a loose substring match on
/// `.get(` would also fire on `declaration_cache.get(&record.gts_type_id)`
/// (checked against the real corpus: this exact shape exists twice in
/// `service.rs`) — a cache lookup *keyed by* the identifier, which is
/// ordinary equality/membership use, not decomposition. Anchoring `.find(`/
/// `.get(`/`.bytes(` to the identifier's own occurrence via
/// [`suffix_immediately_after`] catches the real violation while leaving
/// the lookup alone.
///
/// `mask`/`redact` are deliberately left off `DECOMPOSERS`:
/// [`the_gear_exposes_no_privacy_workflow_operation`] already scans the
/// whole gear for both, unscoped to these four carrier names, which is
/// strictly broader coverage than a carrier-scoped check here would add.
#[test]
fn the_gear_never_decomposes_an_identifier_it_carries() {
    const CARRIERS: &[&str] = &["tenant_id", "subject_id", "resource_id", "gts_type_id"];
    const DECOMPOSERS: &[&str] = &[
        ".split(",
        ".splitn(",
        ".rsplit(",
        ".split_once(",
        ".rsplit_once(",
        ".to_lowercase()",
        ".to_uppercase()",
        ".to_ascii_lowercase()",
        ".to_ascii_uppercase()",
        ".trim(",
        ".trim_start(",
        ".trim_end(",
        ".replace(",
        ".strip_prefix(",
        ".strip_suffix(",
        ".starts_with(",
        ".ends_with(",
        ".chars(",
        ".as_bytes(",
        ".parse(",
        "Uuid::parse_str(",
        "decode(",
        "from_utf8",
        "classif",
    ];
    // Anchored to the identifier's own occurrence (`suffix_immediately_after`),
    // not matched loosely anywhere on the line — see this test's own doc for
    // why `.get(` in particular needs that distinction.
    const ANCHORED_DECOMPOSERS: &[&str] = &[".find(", ".get(", ".bytes("];

    let hits: BTreeSet<String> = gear_sources()
        .iter()
        .flat_map(|(file, text)| {
            let aliases = carrier_aliases(text, CARRIERS);
            text.lines().enumerate().filter_map(move |(i, line)| {
                let carries = aliases.iter().any(|c| contains_ident(line, c));
                let decomposes = DECOMPOSERS.iter().any(|d| line.contains(d))
                    || aliases.iter().any(|c| indexed_immediately_after(line, c))
                    || aliases.iter().any(|c| {
                        ANCHORED_DECOMPOSERS
                            .iter()
                            .any(|d| suffix_immediately_after(line, c, d))
                    });
                (carries && decomposes).then(|| format!("{file}:{}: {}", i + 1, line.trim()))
            })
        })
        .collect();

    assert!(
        hits.is_empty(),
        "identifiers are opaque; these sites decompose one: {hits:#?}"
    );
}

/// `cpt-cf-usage-collector-dod-operational-telemetry-class`'s idempotency-key
/// sentence, **decomposition half only**: "no prefix or segment of it MUST
/// be given meaning."
///
/// **Fix round 1.** The task's own first pass called this
/// clause unpinnable this task for lack of an instrument; that was
/// overstated — adding `idempotency_key` as a carrier to the exact same
/// scan [`the_gear_never_decomposes_an_identifier_it_carries`] already runs
/// is a direct reuse, not new machinery. Kept as its own test rather than
/// folded into that one so a failure names the right clause:
/// `idempotency_key` is telemetry-class, not identifier-class, and
/// conflating the two in one assertion would misattribute a future failure.
///
/// **This does not tick the `DoD`.** The sentence's other half — "the gear
/// compares and never reads" it — is a *read/branch* claim this
/// decomposition scan does not speak to (the same "distinguish branch from
/// comparison" gap recorded against `dod-metadata-opacity`), and the `DoD`'s
/// other sentences (quantity numeric, period bounds/acceptance instant are
/// instants, entry type/origin are closed discriminators) are a type-shape
/// claim resting on SDK-crate declarations this task has no SPI-reaching
/// justification to mark. The `DoD` stays unticked; this closes only the
/// "no test available" objection, not the clause as a whole.
#[test]
fn the_idempotency_key_carries_no_meaningful_prefix_or_segment() {
    const CARRIERS: &[&str] = &["idempotency_key"];
    const DECOMPOSERS: &[&str] = &[
        ".split(",
        ".splitn(",
        ".rsplit(",
        ".split_once(",
        ".rsplit_once(",
        ".trim(",
        ".trim_start(",
        ".trim_end(",
        ".strip_prefix(",
        ".strip_suffix(",
        ".starts_with(",
        ".ends_with(",
        ".chars(",
        ".as_bytes(",
    ];
    const ANCHORED_DECOMPOSERS: &[&str] = &[".find(", ".get(", ".bytes("];

    let hits: BTreeSet<String> = gear_sources()
        .iter()
        .flat_map(|(file, text)| {
            let aliases = carrier_aliases(text, CARRIERS);
            text.lines().enumerate().filter_map(move |(i, line)| {
                let carries = aliases.iter().any(|c| contains_ident(line, c));
                let decomposes = DECOMPOSERS.iter().any(|d| line.contains(d))
                    || aliases.iter().any(|c| indexed_immediately_after(line, c))
                    || aliases.iter().any(|c| {
                        ANCHORED_DECOMPOSERS
                            .iter()
                            .any(|d| suffix_immediately_after(line, c, d))
                    });
                (carries && decomposes).then(|| format!("{file}:{}: {}", i + 1, line.trim()))
            })
        })
        .collect();

    assert!(
        hits.is_empty(),
        "no prefix or segment of the idempotency key may be given meaning; these sites \
         decompose it: {hits:#?}"
    );
}
