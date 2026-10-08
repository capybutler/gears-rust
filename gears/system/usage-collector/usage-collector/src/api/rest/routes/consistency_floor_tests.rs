//! `DESIGN.md` Section 3.10's consistency
//! floor is decided and never published in the generated REST documentation
//! it is supposed to bind. A consumer reading the published contract alone
//! cannot learn it and may assume a stronger guarantee than the gear gives.
//!
//! Scans the route sources directly rather than a route registry: this gear
//! keeps no `READ_ROUTES` constant (verified absent at `107a75733`,
//! `git grep -n 'READ_ROUTES'` returns nothing) — every `.description(...)`
//! is set inline at its own route builder, so the floor has to be published
//! at each one by hand and checked the same way.
//!
//! Covers every route file entry 38 names as the floor's reach: the three
//! query endpoints and the feed endpoint `routes/usage_records.rs` and
//! `routes/usage_feed.rs` carry, plus `routes/reconciliation.rs`'s read
//! surface, plus the SPI's own copy in `usage-collector-sdk/src/plugin_api.rs`
//! — six sites in total, matching `dod-consistency-floor-published`'s own
//! five named sites (three query endpoints, feed, SPI) plus the widened
//! reconciliation surface.
//!
//! **Fix round 1 rewrote this file.** The original pin checked a single
//! six-word substring (`"eventually consistent with no upper bound"`),
//! which a reviewer showed accepts the floor's own negation: deleting three
//! of the four `DoD` claims and the coupling obligation from a route and
//! replacing them with *"Reads are strongly consistent and totally
//! ordered"* left the gear lane green. It also never read `plugin_api.rs`
//! at all, so the SPI site — one of the `DoD`'s five named places — was
//! pinned by nothing; reverting that file alone also left the lane green.
//! And the anchor-then-forward-scan for `.description(...)` had no upper
//! bound, so deleting a route's own description let the scan walk into the
//! next route's and silently credit it instead. All three are fixed below:
//! the full [`usage_records::CONSISTENCY_FLOOR_STATEMENT`] text must be
//! present (not a fragment), `plugin_api.rs` is read the same way
//! `data_classification_tests.rs` already reads it, and the forward scan is
//! bounded at the route's own `.register(...)` call.

use std::path::PathBuf;

/// Read one route file's full source text from this crate's
/// `src/api/rest/routes/`.
fn route_source(file_name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/api/rest/routes")
        .join(file_name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// `usage-collector-sdk/src/plugin_api.rs`'s full source text, read the same
/// way `domain/data_classification_tests.rs:733-739` already reads this
/// exact file (`parent().join("usage-collector-sdk")`) — the original pass
/// through this file claimed the SDK crate was "out of `CARGO_MANIFEST_DIR`"
/// and left the SPI unpinned; that claim was false, contradicted by code
/// four lines above the markers the original pass added, which is why a
/// reviewer brought it back in fix round 1.
fn plugin_api_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the gear crate has a parent directory")
        .join("usage-collector-sdk")
        .join("src")
        .join("plugin_api.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The text passed to the first `.description(...)` call that follows
/// `.operation_id("usage_collector.<op>")` in `source`, bounded at that
/// route's own `.register(...)` call.
///
/// A small paren-matching scan rather than a single-line match: several of
/// these descriptions span multiple lines (backslash-continued string
/// literals), and `.lines().filter(|l| l.contains(".description("))` -- the
/// plan's original skeleton -- only ever sees the opening line of those,
/// never the continuation lines a floor sentence would be appended to.
/// Anchoring on `operation_id` rather than reading every `.description(`
/// in the file is what keeps this from also charging the ingestion/backfill
/// routes in `usage_records.rs`: those are write surfaces DESIGN.md Section
/// 3.10's "Read paths" bullet does not name, so the floor text does not
/// belong on them, and a blind per-file scan would wrongly demand it there
/// too.
///
/// **The `.register(...)` bound was added in fix round 1.** Without
/// it, a route whose own `.description(...)` was deleted left the forward
/// scan for `.description(` walk straight into the *next* route's builder
/// chain and silently adopt its description -- proven by deleting
/// `list_usage_records`'s description entirely and watching the gear lane
/// stay green, crediting `query_aggregated_usage_records`'s text to it.
/// Each route's own chain ends at its own `.register(...)` call, so a
/// `.description(` found at or after that point belongs to someone else's
/// route, and this now panics instead of returning it.
fn description_after_operation_id(source: &str, records_source: &str, op: &str) -> String {
    let anchor = format!(".operation_id(\"usage_collector.{op}\")");
    let anchor_at = source
        .find(&anchor)
        .unwrap_or_else(|| panic!("no `{anchor}` found in this route source"));
    let after = &source[anchor_at..];

    let register_at = after.find(".register(").unwrap_or_else(|| {
        panic!("no .register( found after operation_id {op}; malformed route builder")
    });
    let desc_at = after
        .find(".description(")
        .unwrap_or_else(|| panic!("no .description( found anywhere after operation_id {op}"));
    assert!(
        desc_at < register_at,
        "{op}'s own route builder has no .description(...) before its own .register(...) call; \
         the next `.description(` in the file belongs to a different route, and crediting it \
         here is the exact false pass fix round 1 closed"
    );

    let start = desc_at + ".description(".len();
    let bytes = after.as_bytes();
    let mut depth: i32 = 1;
    let mut idx = start;
    while depth > 0 {
        assert!(
            idx < bytes.len(),
            "unbalanced parens scanning .description( for {op}"
        );
        match bytes[idx] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        idx += 1;
    }
    resolve_description(source, records_source, &after[start..idx - 1])
}

/// Resolve a captured `.description(...)` argument to the text it actually
/// publishes.
///
/// Three of this gear's five read-route descriptions (`usage_records.rs`'s
/// three query routes) pass `format!("... {CONST_NAME} ...")` rather than
/// one inline literal, because inlining the floor text at every call site
/// pushed `register_usage_record_routes` over `clippy::too_many_lines`;
/// `usage_feed.rs` and `reconciliation.rs` do the same via a constant
/// *imported* from `usage_records.rs` (fix round 1 folded their
/// independent copies into one shared, `pub(super)` definition). Reading
/// only the format string's own text would make this pin vacuous for all
/// five -- the text lives in the substituted constant, not in the literal
/// captured at the call site -- so this finds every `{CONST_NAME}`
/// placeholder in the captured argument and substitutes each constant's own
/// source-level definition, found the same way `.description(...)`'s
/// argument was: scanned out of `source`, never assumed.
///
/// Tries `source` (the file the description itself lives in) first, then
/// falls back to `records_source` (`usage_records.rs`, always) -- a
/// constant imported rather than declared locally has no definition to
/// find in its importer's own text, only in `usage_records.rs`'s.
fn resolve_description(source: &str, records_source: &str, raw: &str) -> String {
    let literal = string_literal(raw);
    let mut resolved = normalize_line_continuations(&literal);
    while let Some(open) = resolved.find('{') {
        let Some(close_rel) = resolved[open..].find('}') else {
            break;
        };
        let close = open + close_rel;
        let ident = &resolved[open + 1..close];
        if ident.is_empty() || !ident.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            break;
        }
        let value = const_str_value(source, ident)
            .or_else(|| const_str_value(records_source, ident))
            .unwrap_or_else(|| {
                panic!(
                    "no `const {ident}: &str = \"...\"` found in this file or in usage_records.rs"
                )
            });
        resolved.replace_range(open..=close, &value);
    }
    resolved
}

/// The text between the first and last `"` in `raw` -- a plain string
/// literal, or the sole string argument to a `format!(...)` call. None of
/// this crate's route descriptions pass more than one string argument or an
/// escaped `"`, so this simple a scan is exact for every call site it is
/// used on.
fn string_literal(raw: &str) -> String {
    let first = raw
        .find('"')
        .unwrap_or_else(|| panic!("no opening quote in {raw:?}"));
    let last = raw
        .rfind('"')
        .unwrap_or_else(|| panic!("no closing quote in {raw:?}"));
    assert!(last > first, "no closing quote in {raw:?}");
    raw[first + 1..last].to_owned()
}

/// `const {ident}: &str = "...";`'s own literal value, scanned out of
/// `source` rather than assumed -- the same style of source scan
/// [`description_after_operation_id`] uses for the route builders
/// themselves. Matches both a bare `const` and a `pub(super) const` (a
/// plain substring search on `"const {ident}: &str = \""`, which is present
/// regardless of what visibility modifier precedes it).
fn const_str_value(source: &str, ident: &str) -> Option<String> {
    let anchor = format!("const {ident}: &str = \"");
    let anchor_at = source.find(&anchor)?;
    let start = anchor_at + anchor.len();
    let end = source[start..]
        .find("\";")
        .unwrap_or_else(|| panic!("no closing `\";` found for {ident}"));
    Some(normalize_line_continuations(&source[start..start + end]))
}

/// Collapse a Rust string literal's `\`-newline continuations exactly as
/// `rustc` does -- the backslash, the newline, and every space or tab that
/// opens the next line vanish with no space inserted -- so a phrase this
/// source wraps across a continuation (e.g. "...eventually \" /
/// "consistent...") is still found as one contiguous run of words, matching
/// what the compiled `&str` actually contains rather than this file's own
/// line-wrapped spelling of it.
fn normalize_line_continuations(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('\n' | '\r')) {
            while matches!(chars.peek(), Some('\n' | '\r')) {
                chars.next();
            }
            while matches!(chars.peek(), Some(' ' | '\t')) {
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// `plugin_api.rs`'s own copy of the floor statement, reconstructed from its
/// doc comment rather than assumed: every `///`-prefixed line starting at
/// the one opening "Consistency floor (DESIGN.md Section 3.10): after an
/// ingestion call", up to (not including) the next blank doc line, each
/// stripped of its `/// ` prefix and joined with a single space, then
/// stripped of Markdown code-span backticks -- the one formatting
/// difference `dod-consistency-floor-published`'s "worded identically"
/// requirement does not reach, since a backtick carries no wording of its
/// own (it is how `rustdoc` renders `tenant_id` as code; the REST sites
/// carry the same two words as plain text, since a `.description()` string
/// has no Markdown renderer downstream).
fn spi_floor_statement() -> String {
    let source = plugin_api_source();
    let start_marker = "/// Consistency floor (DESIGN.md Section 3.10): after an ingestion call";
    let start = source.find(start_marker).unwrap_or_else(|| {
        panic!("plugin_api.rs no longer opens its floor quote with the expected sentence")
    });
    let mut text = String::new();
    for line in source[start..].lines() {
        let trimmed = line.trim_start();
        if trimmed == "///" {
            break;
        }
        let Some(rest) = trimmed.strip_prefix("/// ") else {
            break;
        };
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(rest);
    }
    text.replace('`', "")
}

/// Every read-surface route this entry's reach covers, as
/// `(route file, operation id)`.
///
/// `reconciliation.rs`'s `get_reconciliation_metadata` is in this list on
/// the plan's own correction, not DESIGN.md Section 3.10's "Read paths"
/// bullet, which names only raw, aggregate, point lookup and feed: that
/// route's own module doc calls it "the Query Gateway's fourth read path",
/// and nothing in Section 3.10 exempts its counters and watermarks from the
/// same ack-to-visibility lag the other four surfaces publish. Widening the
/// scan here does not create a second, differing floor statement -- the
/// published text is byte-identical to every other site's -- so it cannot
/// trip the `DoD`'s "no competing floor statement" prohibition.
const READ_SURFACES: &[(&str, &str)] = &[
    ("usage_records.rs", "list_usage_records"),
    ("usage_records.rs", "query_aggregated_usage_records"),
    ("usage_records.rs", "get_usage_record"),
    ("usage_feed.rs", "read_usage_feed"),
    ("reconciliation.rs", "get_reconciliation_metadata"),
];

/// `cpt-cf-usage-collector-dod-consistency-floor-published`'s Assertion: "a
/// documentation review over the generated interface description for the
/// three query endpoints, the feed endpoint, and the SPI finds the four
/// claims present and consistently worded, and finds no competing floor
/// statement elsewhere".
///
/// Checks the **whole** [`usage_records::CONSISTENCY_FLOOR_STATEMENT`] text
/// is present at each of the five REST sites, and that `plugin_api.rs`'s own
/// copy equals it exactly (modulo backticks) -- not a six-word fragment.
/// Fix round 1's original pin checked only `"eventually consistent with no
/// upper bound"`, which a reviewer showed accepts a route publishing the
/// floor's own negation (three of the four `DoD` claims and the coupling
/// obligation deleted, replaced with "Reads are strongly consistent and
/// totally ordered") and never read `plugin_api.rs` at all, so a full
/// deletion of its floor section also passed.
///
/// **What this test enforces is agreement, and nothing more.** An earlier
/// revision of this comment said containment of the whole constant
/// "enforces all four claims plus the coupling obligation at once". That
/// was measurably false, and the measurement is worth recording because
/// four fix rounds missed it: five of the six sites obtain their text by
/// `format!("... {CONSISTENCY_FLOOR_STATEMENT} ...")` of the very constant
/// this test reads its expected value from, so containment is logically
/// implied by the substitution. A reviewer rewrote that constant into the
/// floor's own negation, matched `plugin_api.rs` to it word for word, and
/// every lane stayed green with the negation published at all six sites.
/// Every mutation anyone had tried before moved *one site relative to the
/// constant*, which this test does catch; nobody had mutated the constant.
/// That is ruling H51 exactly -- a pin re-sourced from the code it checks.
///
/// So this test's claim is: the six sites agree with each other, and the
/// SPI's copy is byte-equal to the REST sites' (modulo Markdown backticks).
/// Whether what they agree on is *true* is
/// [`every_claim_the_published_floor_makes_is_stated_by_design_md_section_3_10`]'s
/// job, and that test is the only one here whose expected value comes from
/// outside the code under test.
#[test]
fn every_required_site_publishes_the_full_floor_statement() {
    let records_source = route_source("usage_records.rs");
    let floor_statement = const_str_value(&records_source, "CONSISTENCY_FLOOR_STATEMENT")
        .expect("CONSISTENCY_FLOOR_STATEMENT is declared in usage_records.rs");
    assert!(
        !floor_statement.is_empty(),
        "CONSISTENCY_FLOOR_STATEMENT must itself be non-empty, or this whole pin is vacuous"
    );

    let mut undocumented = Vec::new();
    for (file, op) in READ_SURFACES {
        let source = if *file == "usage_records.rs" {
            records_source.clone()
        } else {
            route_source(file)
        };
        let description = description_after_operation_id(&source, &records_source, op);
        if !description.contains(&floor_statement) {
            undocumented.push(format!("{file}::{op}"));
        }
    }
    assert!(
        undocumented.is_empty(),
        "DESIGN.md Section 3.10's full consistency floor statement must be published on each \
         read route's .description(...): {undocumented:#?}"
    );

    let spi_statement = spi_floor_statement();
    assert_eq!(
        spi_statement, floor_statement,
        "UsageCollectorPluginV1's trait doc in usage-collector-sdk/src/plugin_api.rs must carry \
         the identical floor statement (modulo Markdown backticks); dod-consistency-floor-published \
         requires the wording be identical in each published place, SPI included"
    );
}

/// The gear's own `docs/DESIGN.md`, read at test time.
///
/// The plugin's `docs/DESIGN.md` is deliberately not consulted: its
/// `### 3.` headings stop at `3.7 Database schemas & tables` and it has no
/// Section 3.10 at all, so the gear's document is the floor's only source.
fn design_md_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the gear crate has a parent directory")
        .join("docs")
        .join("DESIGN.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// `DESIGN.md`'s Section 3.10 body, from its own heading up to (not
/// including) the next `### ` heading.
fn design_md_section_3_10() -> String {
    const HEADING: &str = "### 3.10 Consistency Contract";
    let source = design_md_source();
    let start = source.find(HEADING).unwrap_or_else(|| {
        panic!(
            "docs/DESIGN.md no longer carries a `{HEADING}` heading; this scan's assumption \
             about the document's structure is wrong, not the document"
        )
    });
    let body = &source[start + HEADING.len()..];
    let end = body
        .find("\n### ")
        .unwrap_or_else(|| panic!("no `### ` heading follows Section 3.10; the bound is unset"));
    body[..end].to_owned()
}

/// Markdown prose reduced to the one form two differently-wrapped,
/// differently-decorated spellings of the same sentence share: emphasis
/// asterisks and code-span backticks dropped, every run of whitespace
/// (including the line wraps and list indentation a Markdown source carries
/// and a Rust string literal does not) collapsed to one space, lowercased.
fn normalize_prose(text: &str) -> String {
    let stripped: String = text.chars().filter(|c| *c != '*' && *c != '`').collect();
    stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Each claim the published floor statement makes, spelled as `DESIGN.md`
/// Section 3.10 itself spells it.
///
/// These are not paraphrases: every entry is a verbatim run of Section
/// 3.10's own prose, which is what makes the test below non-circular. To
/// publish a claim Section 3.10 does not make, an author would have to edit
/// Section 3.10 as well -- and Section 3.10 is the decision, not the code
/// under test.
const FLOOR_CLAIMS_AS_DESIGN_3_10_SPELLS_THEM: &[&str] = &[
    // Floor, ingestion-ack bullet.
    "after an ingestion call returns the persisted entry, that entry is durable",
    "an acknowledged entry can still lose a race before convergence",
    // Floor, read-paths bullet -- the four claims
    // `dod-consistency-floor-published` enumerates.
    "eventually consistent with no upper bound relative to a same-tenant ingestion ack",
    "no monotonic-reads guarantee at the floor.",
    "the floor is per (tenant_id, gts_type_id).",
    "the floor claims no ordering of entries.",
    // Coupling obligation (`dod-staleness-coupling-recorded`'s first MUST).
    "own design document",
    "a breaking change for every coupled consumer",
];

/// The published floor statement's claims are each stated by `DESIGN.md`
/// Section 3.10, read at test time.
///
/// **This is the only assertion in this file whose expected value comes
/// from outside the code under test**, and it exists because
/// [`every_required_site_publishes_the_full_floor_statement`] does not:
/// that test reads `CONSISTENCY_FLOOR_STATEMENT` and then asserts the five
/// REST sites interpolating that same constant contain it, which is true by
/// substitution. Rewriting the constant into the floor's negation shipped
/// the negation at all six sites with every lane green. Section 3.10 is the
/// decision those sites exist to publish, so it is the one source that does
/// not move when the constant does.
///
/// Each entry of [`FLOOR_CLAIMS_AS_DESIGN_3_10_SPELLS_THEM`] is asserted
/// **both ways**: present in Section 3.10 (or the anchor has rotted against
/// a document rewrite and must be re-derived, not quietly dropped) and
/// present in the published constant (or a claim has been dropped, negated
/// or reworded away from its source).
///
/// **Two published clauses are not anchored here, and that is stated rather
/// than engineered around.** The published text condenses Section 3.10, and
/// two of its sentences share no verbatim run with their source beyond
/// `"own design document"`:
///
/// - *"A consumer depending on a tighter bound ... must record that
///   dependency in its own design document"* against Section 3.10's *"That
///   coupling MUST be recorded in the consumer's own design document"*. The
///   `"own design document"` anchor binds the clause's subject but is a
///   fragment, not a whole claim: a sentence could contain it and still say
///   the opposite. Its companion anchor, *"a breaking change for every
///   coupled consumer"*, is a whole claim and does not have that weakness.
/// - *"the Plugin SPI publishes no runtime method for discovering a
///   plugin's ceiling in v1"* against Section 3.10's *"Profile discovery is
///   documentation-only in v1: there is no typed `consistency_profile()`
///   method"*. There is no shared run to anchor on at all. What pins this
///   one is not its wording but its truth:
///   [`neither_published_trait_declares_a_profile_advertisement_method`]
///   re-derives the absence from both published traits on every run.
///
/// **This test requires presence. It does not reject contradiction.** Every
/// one of the eight assertions is a containment check, so it catches a claim
/// **removed**, a claim **negated in place**, and a claim **reworded away
/// from Section 3.10's own spelling** — but a sentence *added* beside the
/// eight leaves all eight present and this test green. Measured, not
/// reasoned: appending *"In practice every read surface is strongly
/// consistent and totally ordered, so no consumer needs to record any
/// coupling at all."* to `CONSISTENCY_FLOOR_STATEMENT` alone reds only
/// `every_required_site_publishes_the_full_floor_statement`, on its SPI
/// equality half — and appending the same sentence to `plugin_api.rs` too,
/// which is what an author updating both copies would do, leaves **all
/// seven tests in this file green with a floor that contradicts itself at
/// all six published sites**.
///
/// Nothing here closes that, and nothing cheap would: detecting a
/// contradiction between two English sentences is not a containment
/// problem. A reader must not infer from a green run that the published
/// floor says *only* what Section 3.10 says — only that everything Section
/// 3.10 says is still in it.
///
/// **Two further bounds, for the same reason.** This test binds the
/// *constant* to Section 3.10, and the test above binds the six *sites* to
/// the constant; neither rules out a site that carries the constant **and**
/// a competing sentence beside it. The `DoD`'s "no competing floor
/// statement elsewhere" clause is checked for the ingestion routes
/// ([`ingestion_route_descriptions_do_not_carry_the_floor_statement`]) and
/// is unpinned for the read routes.
#[test]
fn every_claim_the_published_floor_makes_is_stated_by_design_md_section_3_10() {
    let section = normalize_prose(&design_md_section_3_10());
    assert!(
        section.len() > 2_000,
        "DESIGN.md Section 3.10 extracted to {} characters, which is far short of the section \
         this test exists to read; the extraction is broken, not the document",
        section.len()
    );
    assert!(
        !section.contains("performance patterns"),
        "the Section 3.10 extraction ran past its own bound into Section 3.11; an unbounded \
         read would find every anchor below somewhere in the document and prove nothing"
    );

    let records_source = route_source("usage_records.rs");
    let published = normalize_prose(
        &const_str_value(&records_source, "CONSISTENCY_FLOOR_STATEMENT")
            .expect("CONSISTENCY_FLOOR_STATEMENT is declared in usage_records.rs"),
    );

    let mut rotted = Vec::new();
    let mut unpublished = Vec::new();
    for claim in FLOOR_CLAIMS_AS_DESIGN_3_10_SPELLS_THEM {
        let normalized = normalize_prose(claim);
        if !section.contains(&normalized) {
            rotted.push(*claim);
        }
        if !published.contains(&normalized) {
            unpublished.push(*claim);
        }
    }

    assert!(
        rotted.is_empty(),
        "these anchors are no longer verbatim in DESIGN.md Section 3.10, so they can no longer \
         tell a true published floor from a false one; re-derive each against the section as it \
         now reads rather than deleting it: {rotted:#?}"
    );
    assert!(
        unpublished.is_empty(),
        "CONSISTENCY_FLOOR_STATEMENT no longer states these claims, which DESIGN.md Section \
         3.10 makes; the published floor would be publishing something the gear's own design \
         document does not say: {unpublished:#?}"
    );
}

/// The distinguishing six-word fragment of the floor statement, checked
/// case-insensitively and independently of the whole-constant check in
/// [`ingestion_route_descriptions_do_not_carry_the_floor_statement`] below.
/// Restored after a review round found the whole-constant check alone is
/// strictly weaker for an absence assertion: pasting just this phrase onto
/// an ingestion route's description (without the rest of the constant)
/// left the gear lane green, which is exactly the hazard that test's own
/// doc comment names and the pre-existing predicate used to catch.
const FLOOR_PHRASE: &str = "eventually consistent with no upper bound";

/// The floor binds write-derived state through the ingestion acknowledgement
/// alone (DESIGN.md Section 3.10), not through a query path — so the create
/// and backfill routes in `usage_records.rs` must NOT carry the floor
/// statement. A future edit that pasted it there by reflex would publish a
/// claim the gear does not make (that ingestion itself is "eventually
/// consistent"), which is exactly the kind of second, differing floor
/// statement the `DoD` prohibits. Checked two ways, independently: the
/// whole [`usage_records::CONSISTENCY_FLOOR_STATEMENT`] constant, and
/// [`FLOOR_PHRASE`] alone -- a review round found the whole-constant check
/// by itself is strictly weaker for an absence assertion than the phrase
/// check the original pin used, since a long needle is far easier to
/// not-contain than a short one; pasting the phrase alone (short of the
/// whole constant) onto an ingestion route used to leave the gear lane
/// green, which is exactly the hazard this test exists to catch.
#[test]
fn ingestion_route_descriptions_do_not_carry_the_floor_statement() {
    let records_source = route_source("usage_records.rs");
    let floor_statement = const_str_value(&records_source, "CONSISTENCY_FLOOR_STATEMENT")
        .expect("CONSISTENCY_FLOOR_STATEMENT is declared in usage_records.rs");
    for op in ["create_usage_records", "backfill_usage_records"] {
        let description = description_after_operation_id(&records_source, &records_source, op);
        assert!(
            !description.contains(&floor_statement),
            "{op}'s description must not carry the read-path floor statement; ingestion's own \
             consistency property is acknowledgement durability (with its eventual-dedup \
             qualifier) and the dedup-visibility horizon, not query-path staleness"
        );
        assert!(
            !description
                .to_lowercase()
                .contains(&FLOOR_PHRASE.to_lowercase()),
            "{op}'s description must not carry the read-path floor phrase either, pasted alone \
             and short of the whole constant; ingestion's own consistency property is \
             acknowledgement durability, not query-path staleness"
        );
    }
}

// ── `dod-staleness-coupling-recorded`'s remaining two clauses ──
//
// The floor-statement pin above backs this identifier's second MUST ("The
// gear's published documentation MUST state this obligation where the
// floor is stated") as a side effect: the coupling sentence is part of
// `CONSISTENCY_FLOOR_STATEMENT` itself, so
// `every_required_site_publishes_the_full_floor_statement` already fails if
// it goes missing from any site. The two tests below back the clause's
// other two MUSTs, which that pin does not reach: "the gear MUST NOT offer
// any runtime mechanism for discovering the ceiling" (third MUST) and "a
// consumer that depends on a tighter bound MUST record that dependency...
// naming the plugin, the dimension, and the value" (first MUST). Before
// fix round 1, both were "checked by eye" in a report rather than pinned,
// which a reviewer correctly called out under ruling D1: an absence
// checked once by a human and never re-derived is not the same as an
// absence a running test keeps re-deriving.

/// `usage-collector-sdk/src/api.rs`'s full source text, read the same way
/// [`plugin_api_source`] reads its sibling file.
fn api_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the gear crate has a parent directory")
        .join("usage-collector-sdk")
        .join("src")
        .join("api.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// `dod-staleness-coupling-recorded`'s third `MUST`: "The gear MUST NOT
/// offer any runtime mechanism for discovering the ceiling, since the
/// Plugin SPI carries no profile-advertisement method in v1." Scans both
/// published traits' `async fn` declarations for a name containing
/// `profile` (case-insensitive): neither trait declares one today, and
/// this re-derives that on every build rather than trusting a one-off
/// `grep` typed into a report.
#[test]
fn neither_published_trait_declares_a_profile_advertisement_method() {
    for (file, source) in [
        ("plugin_api.rs", plugin_api_source()),
        ("api.rs", api_source()),
    ] {
        let fn_lines: Vec<&str> = source
            .lines()
            .map(str::trim_start)
            .filter(|line| line.starts_with("async fn ") || line.starts_with("fn "))
            .collect();
        assert!(
            !fn_lines.is_empty(),
            "this scan found no `fn`/`async fn` declaration in {file} at all, which is not true \
             of this trait today -- the scan itself is broken, not the file"
        );
        let offending: Vec<&&str> = fn_lines
            .iter()
            .filter(|line| line.to_lowercase().contains("profile"))
            .collect();
        assert!(
            offending.is_empty(),
            "{file} declares a method naming \"profile\", which \
             dod-staleness-coupling-recorded's third MUST prohibits (no runtime \
             profile-advertisement mechanism in v1): {offending:#?}"
        );
    }
}

/// Every `DESIGN.md` under `gears/`, except this gear's own (and its
/// plugins'), as `(relative path, contents)`.
fn other_gears_design_docs() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, gears_dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, gears_dir, out);
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) != Some("DESIGN.md") {
                continue;
            }
            let rel = path
                .strip_prefix(gears_dir)
                .expect("path is under gears_dir")
                .to_string_lossy()
                .into_owned();
            if rel.starts_with("system/usage-collector/") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            out.push((rel, text));
        }
    }

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent() // usage-collector/ (the gear directory)
        .expect("has a parent")
        .parent() // system/
        .expect("has a parent")
        .parent() // gears/
        .expect("has a parent")
        .parent() // repo root
        .expect("has a parent")
        .to_path_buf();
    let gears_dir = repo_root.join("gears");
    let mut out = Vec::new();
    walk(&gears_dir, &gears_dir, &mut out);
    assert!(
        !out.is_empty(),
        "no other gear's DESIGN.md was found under {}; this scan's path assumptions are wrong, \
         not the repository",
        gears_dir.display()
    );
    out
}

/// True if `needle` (a word or a space-joined phrase) occurs in `haystack`
/// case-insensitively as a whole word/phrase -- the byte immediately
/// before and after the match, if any, is not an ASCII letter or digit.
/// Plain substring containment lets a short unit like `"ms"` match inside
/// ordinary words (`clai`**ms**, `para`**ms**, `ter`**ms**), which is
/// exactly how an earlier version of the scan below was satisfied by 31 of
/// 38 other-gear documents for the wrong reason: a reviewer found the
/// literal `"ms"` matching inside running prose, not a time unit at all.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let haystack_lower = haystack.to_lowercase();
    let needle_lower = needle.to_lowercase();
    if needle_lower.is_empty() {
        return false;
    }
    let bytes = haystack_lower.as_bytes();
    let is_word_byte = |b: u8| b.is_ascii_alphanumeric();
    let mut start = 0usize;
    while let Some(pos) = haystack_lower[start..].find(&needle_lower) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_byte(bytes[abs - 1]);
        let after = abs + needle_lower.len();
        let after_ok = after >= bytes.len() || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
        if start >= haystack_lower.len() {
            break;
        }
    }
    false
}

/// True if `text` states a concrete bound: a run of digits immediately
/// followed, optionally through one space, by a whole time-unit word
/// (`"500ms"`, `"5 minutes"`), or one of a fixed, whole-word-matched list
/// of real percentile spellings (`p50`, `p90`, `p95`, `p99`). Never a bare
/// unit word alone -- that is what let `"ms"` match inside
/// `"terms"`/`"claims"`/`"params"` before this fix, and requiring digit
/// adjacency closes it independently of [`contains_word`]'s boundary fix,
/// since a word-boundary match on `"ms"` alone would still accept a
/// sentence that never states a number at all.
///
/// **Percentiles are a fixed whitelist, not `p` followed by any digits.**
/// An earlier version of this rule accepted `p` immediately before *any*
/// digit run. That is not a collision with this crate's own `@cpt-*:p1`-style
/// colon-attached traceability markers -- a review round measured zero
/// colon-attached `:p<N>` markers anywhere in the scanned `DESIGN.md`
/// corpus, so that was never it. The real collision is this repository's
/// bare, backtick-quoted phase-priority labels -- `` `p1` ``/`` `p2` ``/``
/// `p3` ``/`` `p4` ``, defined in
/// `infrastructure-resource-manager/docs/DESIGN.md:36` ("`p1` describes
/// behavior required in that baseline. `p2` describes agreed follow-up
/// work. `p3` and `p4` are later work.") and used the same way across
/// other gears' design documents -- plus, for the remainder, other
/// incidental `p`-before-a-digit text the old rule could not tell apart
/// from a bound, such as a sequence-diagram participant alias (`P1`, `P2`).
/// Measured: the old rule accepted 1,707 other-gear-corpus paragraphs;
/// restricting to the fixed whitelist below drops that to 409, so 1,298 of
/// them were accepted with no value named at all -- 868 of those carry a
/// bare `p1`-`p4` label, the rest some other incidental `p`+digit text.
fn names_a_bound(text: &str) -> bool {
    const UNITS: &[&str] = &[
        "ms", "s", "sec", "secs", "second", "seconds", "minute", "minutes",
    ];
    const PERCENTILES: &[&str] = &["p50", "p90", "p95", "p99"];
    if PERCENTILES.iter().any(|p| contains_word(text, p)) {
        return true;
    }
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        let mut k = j;
        if k < bytes.len() && bytes[k] == b' ' {
            k += 1;
        }
        let word_start = k;
        while k < bytes.len() && bytes[k].is_ascii_alphabetic() {
            k += 1;
        }
        let word = &lower[word_start..k];
        if UNITS.contains(&word) {
            return true;
        }
        i = j;
    }
    false
}

/// Both the "to" and the "\u{2192}" spellings are listed for each
/// "acceptance" phrase: the gear's own `DESIGN.md` and `PRD.md` publish
/// these three dimensions with an arrow (`"acceptance \u{2192} feed
/// visibility"`), not the word "to", so a consuming document quoting the
/// gear's own vocabulary verbatim used to match neither list -- the worst
/// shape this scan could have, a compliant document failing the pin while
/// its violating twin passed unnoticed. The arrow is written as the escape
/// `\u{2192}` rather than as a literal arrow character, so each string
/// literal's own source bytes stay ASCII (`clippy::non_ascii_literal`
/// denies a literal non-ASCII byte in a string literal on the same terms as
/// a literal section sign).
const TRIGGER_VOCABULARY: &[&str] = &[
    "consistency profile",
    "consistency guarantee",
    "staleness bound",
    "freshness bound",
    "convergence bound",
    "dedup level",
    "acceptance to raw",
    "acceptance to aggregate",
    "acceptance to feed",
    "acceptance \u{2192} raw",
    "acceptance \u{2192} aggregate",
    "acceptance \u{2192} feed",
];

/// A stricter, narrower list than [`TRIGGER_VOCABULARY`]: "this gear's
/// consistency profile" alone triggers scrutiny but does not itself name a
/// dimension, so the two legs cannot collapse into one.
const DIMENSION_VOCABULARY: &[&str] = &[
    "write-path finality",
    "acceptance to raw",
    "acceptance to aggregate",
    "acceptance to feed",
    "acceptance \u{2192} raw",
    "acceptance \u{2192} aggregate",
    "acceptance \u{2192} feed",
    "dedup level",
    "convergence bound",
    "retention floor",
    "ingestion batching",
];

/// What the coupling scan makes of one paragraph.
#[derive(Debug, PartialEq, Eq)]
enum CouplingVerdict {
    /// Does not both name this gear and reach [`TRIGGER_VOCABULARY`].
    NotScrutinized,
    /// Reaches the trigger and names a plugin, a dimension, and a bound.
    Compliant,
    /// Reaches the trigger and is missing at least one of the three.
    Violating {
        names_plugin: bool,
        names_dimension: bool,
        names_value: bool,
    },
}

/// The scan's whole per-paragraph rule, as a function so the corpus walk
/// below and the fixture pin beside it exercise the same code rather than
/// two spellings of it.
fn coupling_verdict(paragraph: &str) -> CouplingVerdict {
    let mentions_gear =
        contains_word(paragraph, "usage-collector") || contains_word(paragraph, "usage_collector");
    if !mentions_gear {
        return CouplingVerdict::NotScrutinized;
    }
    let triggers = TRIGGER_VOCABULARY
        .iter()
        .any(|kw| contains_word(paragraph, kw));
    if !triggers {
        return CouplingVerdict::NotScrutinized;
    }
    let names_plugin = contains_word(paragraph, "plugin");
    let names_dimension = DIMENSION_VOCABULARY
        .iter()
        .any(|kw| contains_word(paragraph, kw));
    let names_value = names_a_bound(paragraph);
    if names_plugin && names_dimension && names_value {
        CouplingVerdict::Compliant
    } else {
        CouplingVerdict::Violating {
            names_plugin,
            names_dimension,
            names_value,
        }
    }
}

/// `dod-staleness-coupling-recorded`'s first `MUST`: "A consumer that
/// depends on a bound tighter than the floor MUST record that dependency
/// in its own design document, naming the plugin, the dimension, and the
/// value it relies on."
///
/// Pinned as an absence rather than asserted once in a report: scans every
/// *other* gear's `DESIGN.md`, paragraph by paragraph, for one that both
/// names this gear and uses general consistency-coupling vocabulary
/// ([`TRIGGER_VOCABULARY`]); a paragraph meeting that bar must also name a
/// plugin, name one of this gear's specific published profile dimensions
/// ([`DIMENSION_VOCABULARY`]), and state a concrete bound
/// ([`names_a_bound`]). All three checks are whole-word matches
/// ([`contains_word`]), not substrings, and apply to the *triggering
/// paragraph* rather than the whole document, so a coupling claim and its
/// justification have to sit together rather than merely coexist anywhere
/// in a long file. The rule itself lives in [`coupling_verdict`].
///
/// Anchoring to the paragraph, rather than the whole document, is also
/// what keeps a document like `policy-engine/docs/DESIGN.md` -- one of
/// only two documents in the corpus that mention this gear at all --
/// out of scope honestly rather than by accident: its one mention of
/// "usage-collector" sits in an architecture-comparison table with no
/// consistency vocabulary anywhere near it, so it never reaches the
/// trigger at all, and is not exempted by an incidental "plugin"/"ms"
/// match the way the whole-document version of this check was.
///
/// **This test alone cannot exercise its own vocabulary.** Measured at
/// closeout: the corpus is 38 documents and 13,385 paragraphs, 2 of which
/// mention this gear and **0** of which reach the trigger. Replacing every
/// [`TRIGGER_VOCABULARY`] entry with one never-occurring string therefore
/// left it green. Two siblings close that gap --
/// [`the_coupling_scan_discriminates_on_a_checked_in_fixture_pair`] runs
/// [`coupling_verdict`] over hand-written paragraphs, and
/// [`every_vocabulary_phrase_is_either_published_by_the_gear_or_declared_unpublished`]
/// anchors each phrase to the gear's own documents.
#[test]
fn every_consuming_design_doc_that_couples_to_a_tighter_bound_names_it() {
    let mut offending = Vec::new();
    for (path, text) in other_gears_design_docs() {
        for paragraph in text.split("\n\n") {
            if let CouplingVerdict::Violating {
                names_plugin,
                names_dimension,
                names_value,
            } = coupling_verdict(paragraph)
            {
                offending.push(format!(
                    "{path}: plugin={names_plugin} dimension={names_dimension} \
                     value={names_value}: {paragraph:?}"
                ));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "these DESIGN.md paragraphs use usage-collector consistency-coupling vocabulary \
         without naming the plugin, the dimension, and a concrete bound together, as \
         dod-staleness-coupling-recorded's first MUST requires of a consumer coupling to a \
         tighter bound: {offending:#?}"
    );
}

/// A paragraph that couples to a tighter bound without naming what it
/// couples to -- exactly what `dod-staleness-coupling-recorded`'s first
/// MUST forbids.
///
/// Written by hand, not generated from [`TRIGGER_VOCABULARY`]. Building a
/// fixture out of the constant under test would make the pin below say only
/// that the constant equals itself -- ruling H51 again, in the same file
/// that already carries one instance of it. These two paragraphs are an
/// independent spelling of the same phrases, so corrupting either side reds
/// the run.
const VIOLATING_COUPLING_FIXTURE: &str = "The quota service reads from usage-collector and \
    depends on its consistency profile being tight enough for admission decisions.";

/// The compliant twin of [`VIOLATING_COUPLING_FIXTURE`]: the same coupling,
/// recorded the way the clause requires -- naming the plugin, naming one of
/// the gear's published profile dimensions, and stating a concrete bound.
const COMPLIANT_COUPLING_FIXTURE: &str = "The quota service couples to the timescaledb plugin \
    acceptance to feed dimension of the usage-collector consistency profile, relying on a \
    500ms bound.";

/// A paragraph that names the gear without any coupling vocabulary near it
/// -- the shape `policy-engine/docs/DESIGN.md` has, and the reason the scan
/// anchors on a paragraph rather than a whole document.
const UNSCRUTINIZED_COUPLING_FIXTURE: &str = "Compared with usage-collector, this gear keeps \
    its own write path and does not depend on a storage plugin at all.";

/// The scan discriminates on real input, over a fixture pair checked in
/// beside it.
///
/// **Why this exists.** The corpus the scan walks is 38 other-gear
/// `DESIGN.md` files; 2 of their paragraphs mention this gear and **0**
/// reach the trigger. So the whole of
/// [`every_consuming_design_doc_that_couples_to_a_tighter_bound_names_it`]
/// is an assertion about an empty set, and a reviewer showed what that
/// costs: replacing all twelve [`TRIGGER_VOCABULARY`] entries with one
/// never-occurring string left that test green. The pinned absence is real
/// under ruling D1, but nothing guarded the vocabulary that makes it work,
/// so a typo in any trigger phrase, dimension phrase or unit was
/// undetectable.
///
/// This runs the same [`coupling_verdict`] the corpus walk runs, over three
/// hand-written paragraphs, and so exercises the trigger leg, the dimension
/// leg, the plugin leg and [`names_a_bound`] on text that actually reaches
/// them.
///
/// **What it still does not reach, stated rather than papered over:** three
/// paragraphs cannot exercise the two lists' 23 entries, which are 15
/// distinct phrases. These fixtures reach **2 of those 15** --
/// `"consistency profile"` (the trigger leg) and `"acceptance to feed"` (the
/// dimension leg) -- plus the `"500ms"` unit shape, which is
/// [`names_a_bound`]'s and not vocabulary at all. The other **13 distinct
/// phrases** are covered by their own pin instead --
/// [`every_vocabulary_phrase_is_either_published_by_the_gear_or_declared_unpublished`]
/// -- which anchors every one of the 23 entries to the gear's own documents
/// rather than to a fixture.
#[test]
fn the_coupling_scan_discriminates_on_a_checked_in_fixture_pair() {
    assert!(
        matches!(
            coupling_verdict(VIOLATING_COUPLING_FIXTURE),
            CouplingVerdict::Violating { .. }
        ),
        "a paragraph that names this gear and its consistency profile, without naming a \
         plugin, a dimension or a bound, must be flagged; it is not, so the trigger leg or \
         one of the three requirement legs has stopped working: {:?}",
        coupling_verdict(VIOLATING_COUPLING_FIXTURE)
    );
    assert_eq!(
        coupling_verdict(COMPLIANT_COUPLING_FIXTURE),
        CouplingVerdict::Compliant,
        "the compliant twin names the plugin, a published dimension and a concrete bound, so \
         it must pass; flagging it would mean this scan fails documents that do exactly what \
         the clause asks"
    );
    assert_eq!(
        coupling_verdict(UNSCRUTINIZED_COUPLING_FIXTURE),
        CouplingVerdict::NotScrutinized,
        "a paragraph naming this gear with no coupling vocabulary near it must not be \
         scrutinized at all, or every passing mention in the repository becomes a finding"
    );
}

/// Vocabulary entries the gear's own documents do not publish verbatim.
///
/// Two kinds, and neither is an oversight:
///
/// - `"consistency guarantee"` and `"staleness bound"` are the generic
///   words a *consuming* document is likely to reach for instead of this
///   gear's own. They are on the trigger list precisely because they are
///   not the gear's vocabulary.
/// - The three `"acceptance to ..."` spellings are ASCII transliterations
///   of the gear's own arrow spelling, carried so a consumer that wrote
///   "to" rather than the arrow still trips the scan.
///
/// Asserted in both directions by the test below: a row here that *becomes*
/// published must be removed from this list, and an entry that stops being
/// published must be added to it.
const VOCABULARY_WITHOUT_A_PUBLISHED_SOURCE: &[&str] = &[
    "consistency guarantee",
    "staleness bound",
    "acceptance to raw",
    "acceptance to aggregate",
    "acceptance to feed",
];

/// Every phrase the coupling scan keys on is either published verbatim by
/// the gear's own `DESIGN.md`/`PRD.md`, or declared in
/// [`VOCABULARY_WITHOUT_A_PUBLISHED_SOURCE`] with a reason.
///
/// This is the half of the vocabulary problem a fixture pair cannot reach.
///
/// **Counted two ways, because conflating them is its own defect.** The two
/// lists hold **23 entries** between them, which are **15 distinct
/// phrases** — eight phrases sit on both lists. This test walks all 23
/// entries and so covers all 15 distinct phrases, anchoring each to a source
/// outside the test file; replacing an entry with a never-occurring string
/// — the mutation that used to leave the corpus scan green — reds here.
/// [`the_coupling_scan_discriminates_on_a_checked_in_fixture_pair`] reaches
/// **2 of those 15 distinct phrases** (`"consistency profile"` and
/// `"acceptance to feed"`), so **13 distinct phrases are pinned here and
/// nowhere else**. Of the 15 distinct, **10 are published** by the gear's
/// own documents and **5 are declared** in
/// [`VOCABULARY_WITHOUT_A_PUBLISHED_SOURCE`]; in entry terms that split is
/// 15 published and 8 declared. An earlier revision of this sentence said
/// the fixtures exercise "three phrases" and this anchors "the other
/// fifteen", which matched neither counting — 15 is the published *entry*
/// count, not the number of phrases left over.
///
/// **Its limit, stated:** this pins that each phrase is *the gear's own
/// published wording*. It cannot pin that the list is *complete* — a
/// coupling phrase a consumer might use and neither list names is still
/// invisible, and no instrument available here can enumerate what a future
/// document will say.
#[test]
fn every_vocabulary_phrase_is_either_published_by_the_gear_or_declared_unpublished() {
    let docs_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the gear crate has a parent directory")
        .join("docs");
    let mut corpus = String::new();
    for name in ["DESIGN.md", "PRD.md"] {
        let path = docs_dir.join(name);
        corpus.push_str(&normalize_prose(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())),
        ));
        corpus.push(' ');
    }
    assert!(
        corpus.len() > 100_000,
        "the gear's DESIGN.md + PRD.md normalized to {} characters, which is far short of the \
         documents this pin exists to read; the read is broken, not the documents",
        corpus.len()
    );

    let declared_unpublished: std::collections::BTreeSet<&str> =
        VOCABULARY_WITHOUT_A_PUBLISHED_SOURCE
            .iter()
            .copied()
            .collect();
    let mut unaccounted = Vec::new();
    let mut wrongly_declared = Vec::new();
    for phrase in TRIGGER_VOCABULARY.iter().chain(DIMENSION_VOCABULARY) {
        let published = corpus.contains(&normalize_prose(phrase));
        match (published, declared_unpublished.contains(phrase)) {
            (false, false) => unaccounted.push(*phrase),
            (true, true) => wrongly_declared.push(*phrase),
            _ => {}
        }
    }

    assert!(
        unaccounted.is_empty(),
        "these coupling-scan phrases are neither published verbatim by the gear's own \
         DESIGN.md/PRD.md nor declared in VOCABULARY_WITHOUT_A_PUBLISHED_SOURCE. A phrase \
         with no source is a phrase nothing can tell apart from a typo: {unaccounted:#?}"
    );
    assert!(
        wrongly_declared.is_empty(),
        "these phrases are declared as having no published source, but the gear's own \
         documents publish them verbatim today; delete the row rather than leave the \
         declaration standing against the measurement: {wrongly_declared:#?}"
    );
}
