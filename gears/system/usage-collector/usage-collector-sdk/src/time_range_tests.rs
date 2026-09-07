use time::{Duration, OffsetDateTime, UtcOffset};

use crate::error::UsageCollectorError;
use crate::time_range::TimeRange;

fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).expect("in-range unix timestamp")
}

#[test]
fn new_accepts_an_ordered_range() {
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("ordered range");
    assert_eq!(range.lower_inclusive(), at(1_700_000_000));
    assert_eq!(range.upper_exclusive(), at(1_700_003_600));
}

#[test]
fn new_rejects_an_inverted_range() {
    let err = TimeRange::new(at(1_700_003_600), at(1_700_000_000))
        .expect_err("to < from must be rejected");
    let UsageCollectorError::InvalidArgument { field, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "time_range");
}

#[test]
fn new_rejects_an_empty_range() {
    // `from == to` selects nothing at all: the predicate is
    // `from <= window_end < to`. Accepting it would report "no usage" for
    // what is really a caller passing one instant twice.
    TimeRange::new(at(1_700_000_000), at(1_700_000_000)).expect_err("from == to must be rejected");
}

#[test]
fn new_normalizes_both_bounds_to_utc() {
    let offset = UtcOffset::from_hms(5, 30, 0).expect("valid offset");
    let range = TimeRange::new(
        at(1_700_000_000).to_offset(offset),
        at(1_700_003_600).to_offset(offset),
    )
    .expect("ordered range");
    assert_eq!(range.lower_inclusive().offset(), UtcOffset::UTC);
    assert_eq!(range.upper_exclusive().offset(), UtcOffset::UTC);
    // Normalization moves the offset, never the instant, on both bounds.
    assert_eq!(range.lower_inclusive(), at(1_700_000_000));
    assert_eq!(range.upper_exclusive(), at(1_700_003_600));
}

#[test]
fn selection_is_inclusive_at_the_lower_bound() {
    // This inclusive lower bound is also what selects a point event sitting
    // on a range boundary: a point event has window_start == window_end, so
    // `contains_window_end` — which reads only window_end — cannot tell it
    // apart from any other entry ending here, and needs no case of its own.
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    assert!(range.contains_window_end(at(1_700_000_000)));
}

#[test]
fn selection_is_exclusive_at_the_upper_bound() {
    // ADR-0014: an entry whose period ends exactly on the upper bound
    // belongs to the NEXT range. This is what makes adjacent ranges sum
    // without double counting.
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    assert!(!range.contains_window_end(at(1_700_003_600)));
}

#[test]
fn selection_holds_inside_the_range() {
    // Every other test passes a window_end equal to a bound, or past the
    // upper one. Nothing pins the ordinary case: a window_end strictly
    // between from and to.
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    assert!(range.contains_window_end(at(1_700_001_800)));
}

#[test]
fn an_entry_ending_before_the_lower_bound_is_not_selected() {
    // Without this, a predicate that only checks the upper bound (dropping
    // the `from <=` conjunct) is indistinguishable from the correct one.
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    assert!(!range.contains_window_end(at(1_699_999_999)));
}

#[test]
fn an_entry_wider_than_the_range_is_selected_by_exactly_one_range() {
    // The predicate reads window_end and nothing else, so an entry covering
    // [range.from - 1h, range.to + 1h) is invisible to `range` and to the
    // range before it — but ADR-0014's partition property means some range
    // does select it: the one whose upper bound reaches its window_end.
    let range = TimeRange::new(at(1_700_000_000), at(1_700_003_600)).expect("range");
    let wide_entry_end = range.upper_exclusive() + Duration::hours(1);
    assert!(!range.contains_window_end(wide_entry_end));

    let earlier = TimeRange::new(at(1_699_996_400), at(1_700_000_000)).expect("range");
    assert!(!earlier.contains_window_end(wide_entry_end));

    let next = TimeRange::new(range.upper_exclusive(), wide_entry_end + Duration::hours(1))
        .expect("range");
    assert!(next.contains_window_end(wide_entry_end));
}
