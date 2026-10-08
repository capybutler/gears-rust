//! The traceability graph's assertions.

use super::{
    Marker, UNBACKED_TICKS, UNRESOLVED_IDENTIFIERS, UNRESOLVED_INSTANCES, declared_identifiers,
    declared_instances, markers, ticked_identifiers,
};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn every_marker_identifier_resolves_to_a_declared_one_less_the_excused_rows() {
    let declared = declared_identifiers();
    let unresolved: BTreeSet<String> = markers()
        .into_iter()
        .map(|m| m.identifier)
        .filter(|id| !declared.contains(id))
        .collect();

    let excused: BTreeSet<String> = UNRESOLVED_IDENTIFIERS
        .iter()
        .map(|(id, _)| (*id).to_owned())
        .collect();

    assert_eq!(
        unresolved, excused,
        "left is what the code names and no document declares; right is \
         UNRESOLVED_IDENTIFIERS. An identifier only on the left is a marker \
         pointing at nothing. One only on the right is a row that has been \
         paid off and must be deleted."
    );
}

/// A `@cpt-dod:` marker naming a `…-flow-…` identifier resolves perfectly
/// well and is still wrong. Resolution is about existence; this is about
/// agreement.
///
/// Two things keep a marker out of the comparison, and the closure below
/// applies them in the opposite order to the one that reads naturally.
/// `find(...)?` runs *first*: a marker whose cited identifier carries none of
/// the four kind segments has nothing to check against and is dropped there,
/// whatever its own kind. Only what survives that reaches the `begin`/`end`
/// early return, which is the exemption by design — a span's kind is judged
/// on its identifier's own segment, never on the literal `begin`/`end` token.
///
/// So the skip is not a set to *add* to the exemption; the two overlap.
/// Measured over today's corpus: `find(...)?` drops 103 of the 838 marker
/// sites, and 20 of those 103 are `begin`/`end` sites the exemption would
/// have dropped a line later anyway. The part that is genuinely further than
/// the exemption is the remaining 83 non-span sites.
///
/// Counted by identifier rather than by site, that further set is wider than
/// `flow`-kind-only: 10 distinct `flow`-kind identifiers (the
/// `cpt-cf-uc-plugin-seq-*` family, `cpt-cf-usage-collector-seq-read-feed`,
/// `…-component-feed-gateway`, and `…-principle-cursor-gateway-ownership`)
/// plus 35 distinct `dod`-kind identifiers citing a requirement, NFR,
/// principle, ADR, constraint, contract, component, entity or sequence rather
/// than a `dod-` segment of their own (`fr-ingestion`,
/// `principle-fail-closed`, `nfr-availability`, and the like) — 45 distinct
/// identifiers, the two kinds disjoint. That is a property of the document
/// vocabulary rather than of how many marker sites cite them, so there is
/// nothing here for this test to agree or disagree with regardless of corpus
/// size. The 20 `begin`/`end` sites contribute no identifier of their own:
/// the four they cite are already among the 45. An earlier revision of this
/// comment counted only the `flow`-kind subset and so understated the skip;
/// the `dod`-kind citations were always skipped the same way, just never
/// added in. The four-way search order (`dod`, `algo`, `flow`, `state`) is
/// unambiguous today: no identifier in the corpus contains more than one of
/// these segments, so which one `find` happens to hit first never matters in
/// practice.
#[test]
fn every_marker_kind_agrees_with_the_identifier_it_names() {
    // `begin`/`end` are span kinds and carry whatever kind the identifier
    // does, so they are judged on the identifier's own segment instead.
    let mismatched: Vec<String> = markers()
        .into_iter()
        .filter_map(|m| {
            // A plain substring test, not a parsed segment list — fragile if
            // an identifier ever embeds one of these words as a fragment
            // rather than its own `-kind-` segment. No identifier in
            // today's corpus does.
            let segment = ["dod", "algo", "flow", "state"]
                .into_iter()
                .find(|seg| m.identifier.contains(&format!("-{seg}-")))?;
            if m.kind == "begin" || m.kind == "end" {
                return None;
            }
            let expected = m.kind.as_str();
            (expected != segment).then(|| {
                format!(
                    "{}:{} @cpt-{} names {} (a `{}` identifier)",
                    m.file, m.line, m.kind, m.identifier, segment
                )
            })
        })
        .collect();

    assert!(
        mismatched.is_empty(),
        "a marker's kind must match its identifier's own kind segment: {mismatched:#?}"
    );
}

/// `(file, identifier, instance)` -> that key's `(line, is_begin)` events.
type SpanEventsByKey = BTreeMap<(String, String, String), Vec<(usize, bool)>>;

/// Balanced *per identifier and per instance*, correctly ordered, and
/// *non-crossing*.
///
/// A bare total catches none of the four failures that matter: an `end`
/// before its `begin`, two `begin`s against one `end`, a rebind that moved
/// one half of a pair, and two same-key spans whose begins/ends interleave
/// (`begin A, begin B, end A, end B`). That last one is why this merges a
/// key's `begin`/`end` lines into one line-ordered sequence and requires it
/// to alternate strictly `begin, end, begin, end, …`, rather than pairing
/// `begins` against `ends` position-for-position: a naive sorted zip would
/// pair `(beginA, endA)` and `(beginB, endB)` by position and find both
/// individually well-ordered, missing that the two spans actually overlap.
/// Some keys in today's corpus carry more than one pair — a deliberate
/// pattern: one instance legitimately covering several disjoint, sequential
/// stretches of code that realize the same step. The per-key merge-and-
/// alternate check above is exactly what makes that pattern distinguishable
/// from the interleaved-overlap fault, rather than merely tolerated by
/// accident. The crossing case itself is still latent rather than measured
/// here — kept anyway because every slice-9 rebind task moves enough span
/// markers that introducing one is a real risk. §4a's own baseline check is
/// a bare total.
#[test]
fn every_span_is_balanced_and_well_ordered_per_instance() {
    let mut events: SpanEventsByKey = BTreeMap::new();

    for Marker {
        file,
        line,
        kind,
        identifier,
        instance,
        ..
    } in markers()
    {
        let Some(instance) = instance else { continue };
        let key = (file, identifier, instance);
        match kind.as_str() {
            "begin" => events.entry(key).or_default().push((line, true)),
            "end" => events.entry(key).or_default().push((line, false)),
            _ => {}
        }
    }

    let mut faults = Vec::new();
    for (key, evs) in &mut events {
        // `markers()` already yields each file's lines in ascending order,
        // but sort defensively so a future change to scan order can't
        // silently break the alternation check below.
        evs.sort_by_key(|(line, _)| *line);

        if evs.len() % 2 != 0 {
            faults.push(format!(
                "{key:?}: {} begin/end events total (odd — unpaired)",
                evs.len()
            ));
            continue;
        }
        for (i, pair) in evs.chunks(2).enumerate() {
            let (b_line, b_is_begin) = pair[0];
            let (e_line, e_is_begin) = pair[1];
            if !b_is_begin || e_is_begin {
                faults.push(format!(
                    "{key:?}: pair {i} is not begin-then-end (lines {b_line}, {e_line})"
                ));
            } else if b_line >= e_line {
                faults.push(format!(
                    "{key:?}: begin at line {b_line} is not before end at line {e_line}"
                ));
            }
        }
    }

    assert!(faults.is_empty(), "span faults: {faults:#?}");
}

/// Ruling E18's convention, turned into an assertion: a tick is backed by a
/// code marker.
///
/// Excused for now by the same exactly-asserted discipline, because this
/// direction has a large standing population at slice 9's start — every box
/// a pre-E18 slice ticked, and every rollup identifier, which is unmarkable
/// by construction (no `featstatus` or `feature` marker kind exists; six
/// kinds do: begin, end, dod, algo, flow, state).
#[test]
fn every_ticked_identifier_is_backed_by_a_marker_less_the_excused_rows() {
    let marked: BTreeSet<String> = markers().into_iter().map(|m| m.identifier).collect();
    let unbacked: BTreeSet<String> = ticked_identifiers()
        .into_iter()
        .filter(|id| !marked.contains(id))
        .collect();

    let excused: BTreeSet<String> = UNBACKED_TICKS
        .iter()
        .map(|(id, _)| (*id).to_owned())
        .collect();

    assert_eq!(
        unbacked, excused,
        "left is ticked-with-no-marker; right is UNBACKED_TICKS. A new \
         identifier on the left means a tick was placed without a marker, or \
         a marker was deleted out from under a tick that it earned."
    );
}

#[test]
fn every_span_instance_resolves_to_a_declared_step_less_the_excused_rows() {
    let declared = declared_instances();
    let unresolved: BTreeSet<String> = markers()
        .into_iter()
        .filter_map(|m| m.instance)
        .filter(|i| !declared.contains(i))
        .collect();

    let excused: BTreeSet<String> = UNRESOLVED_INSTANCES
        .iter()
        .map(|(i, _)| (*i).to_owned())
        .collect();

    assert_eq!(
        unresolved, excused,
        "left is code; right is UNRESOLVED_INSTANCES."
    );
}

/// Every marker in this subtree names this subtree's own namespaces.
///
/// Measured at slice 9's start: `cpt-cf-usage-collector` and
/// `cpt-cf-uc-plugin` and nothing else. A `cpt-cf-file-storage-*` marker here
/// would resolve against another gear's documents under a repo-wide oracle
/// and against nothing under this one; both are defects, and neither was
/// asserted before.
#[test]
fn no_marker_names_another_gears_identifier_namespace() {
    let foreign: Vec<String> = markers()
        .into_iter()
        .filter(|m| {
            !m.identifier.starts_with("cpt-cf-usage-collector-")
                && !m.identifier.starts_with("cpt-cf-uc-plugin-")
        })
        .map(|m| format!("{}:{} {}", m.file, m.line, m.identifier))
        .collect();

    assert!(
        foreign.is_empty(),
        "foreign-namespace markers: {foreign:#?}"
    );
}
