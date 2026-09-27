//! The paginated feed read the feed checks share.
//!
//! A feed page is the one read surface in this SPI a check may not take at
//! its word: `limit` bounds what a page *carries* and promises nothing
//! about everything settled fitting on one, so a short page is conforming
//! and a check that read once would report a backend paging in small steps
//! as having lost entries. Every read here is therefore **followed** — the
//! cursor is chased to a fixpoint — and the fixpoint, rather than a page
//! count, is what says the walk reached the head.
//!
//! This module holds that loop once. It landed inside
//! [`feed_snapshot_and_replay`](super::checks::feed_snapshot_and_replay()),
//! the first check to page the feed, and moved here when the second one
//! needed it: DESIGN §3.3 tabulates five feed checks, and a walk
//! re-implemented per check is five chances for two checks to disagree
//! about what "reached the head" means.
//!
//! **It is a reader, not a check.** Nothing here reports a violation. The
//! failures it can see — a refused page, a live page with no continuation,
//! a cursor that never stands still — come back as an `Err` carrying a
//! ready-to-report detail, and the check that dispatched the walk decides
//! which name to file it under.

use toolkit_odata::ast;
use uuid::Uuid;

use crate::feed::{FeedPosition, FeedStart};
use crate::models::{MeterTypeId, UsageRecord};
use crate::plugin_api::UsageCollectorPluginV1;

/// How many feed pages any one walk will follow before it gives up.
///
/// A walk's real exit is the cursor standing still: a page whose `next`
/// repeats the position the read began at has delivered everything after
/// it. This budget bounds the **loop**, not the read, and it is why a
/// backend whose cursor never advances is reported rather than left to
/// spin.
///
/// One figure for every check rather than one each, because it is not a
/// property of any check's ledger: no conforming backend reaches it at any
/// limit the suite dispatches, and a check that did reach it would be
/// reporting the same defect whichever ledger it walked.
pub const FEED_WALK_PAGES: usize = 256;

/// What one live, unbounded walk over the feed observed.
pub struct FeedWalk {
    /// Every entry the walk was handed, in the order the pages carried them.
    pub delivered: Vec<UsageRecord>,
    /// The position the walk stopped at.
    ///
    /// **Not an `Option`, and that is the point.** A live read carries a
    /// continuation on every page, short and empty pages included; an
    /// absent one is reserved for a bounded replay reaching its `until`.
    /// [`follow`] reports a live page carrying none as a failed walk, so by
    /// the time a probe holds a `FeedWalk` there is a position. Three
    /// probes in `feed-snapshot-and-replay` used to guard for its absence
    /// and the guards were measured as unreachable: no backend outcome
    /// could make them fire, so they were reports that could never be read.
    /// This type is where that argument lives.
    pub stopped_at: FeedPosition,
    /// How many pages the walk read. Reported in a detail so a plugin
    /// author can see where a scan was paused; asserted nowhere, because
    /// how a conforming backend divides a span into pages is its own
    /// business — and because an assertion on it would be false on the
    /// second run of a suite that never removes an entry.
    pub pages: usize,
}

/// What one replay bounded by an `until` observed.
pub struct BoundedReplay {
    /// Every entry the replay was handed, in the order the pages carried
    /// them.
    pub delivered: Vec<UsageRecord>,
    /// Whether the last page carried no continuation, which is the one
    /// thing that says a bounded replay reached its `until`.
    pub closed: bool,
}

/// When [`follow`] stops following the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkStop {
    /// At the head: the walk follows the cursor until it stands still.
    AtTheHead,
    /// As soon as a page has delivered anything, or at the head if none
    /// ever does. This is what pauses a scan mid-way without predicting how
    /// a backend pages.
    FirstDelivery,
}

/// Follows a live, unbounded feed read to [`WalkStop`].
///
/// `Err` carries a ready-to-report detail, including for the one shape
/// [`FeedWalk::stopped_at`] rules out: a live page with no continuation.
pub async fn feed_walk(
    plugin: &dyn UsageCollectorPluginV1,
    subscription: &[MeterTypeId],
    scope: &ast::Expr,
    start: FeedStart<FeedPosition>,
    limit: u64,
    stop: WalkStop,
) -> Result<FeedWalk, String> {
    let (delivered, next, pages) =
        follow(plugin, subscription, scope, start, None, limit, stop).await?;
    let stopped_at = next.ok_or_else(|| {
        "a live, unbounded feed read ended with no continuation to resume from. A live read \
         carries one on every page, short and empty pages included; an absent one is reserved \
         for a bounded replay reaching its `until`, and this read sent none."
            .to_owned()
    })?;
    Ok(FeedWalk {
        delivered,
        stopped_at,
        pages,
    })
}

/// Follows a replay bounded by `until` until it closes or its cursor stands
/// still.
///
/// `Err` carries a ready-to-report detail.
pub async fn bounded_replay(
    plugin: &dyn UsageCollectorPluginV1,
    subscription: &[MeterTypeId],
    scope: &ast::Expr,
    until: FeedPosition,
    limit: u64,
) -> Result<BoundedReplay, String> {
    let (delivered, next, _pages) = follow(
        plugin,
        subscription,
        scope,
        FeedStart::Oldest,
        Some(until),
        limit,
        WalkStop::AtTheHead,
    )
    .await?;
    Ok(BoundedReplay {
        delivered,
        closed: next.is_none(),
    })
}

/// The loop both of the two above are built on.
///
/// The subscription is the caller's, and a check that reads the feed passes
/// a meter of its own for the reason
/// [`check_meter`](super::fixtures::check_meter) exists: a feed read
/// selects by meter, so a subscription naming the suite's shared meter
/// would carry whatever the other checks left on it and what the check
/// observed would turn on the order [`run_all`](super::run_all) happened to
/// dispatch in.
///
/// The loop's exit is the cursor standing still rather than a page coming
/// back short, because **a short page is conforming**: `limit` bounds what
/// a page carries and promises nothing about everything settled fitting on
/// one, and DESIGN's `feed-retention-refusal` row speaks of a cursor
/// *"refused rather than served as a short page"*, so short pages are a
/// shape the contract knows. [`FEED_WALK_PAGES`] bounds the loop so a
/// backend whose cursor never advances is reported rather than followed
/// forever.
///
/// Returns what the pages carried, the continuation the last of them
/// carried, and how many pages were read. `Err` carries a ready-to-report
/// detail.
async fn follow(
    plugin: &dyn UsageCollectorPluginV1,
    subscription: &[MeterTypeId],
    scope: &ast::Expr,
    start: FeedStart<FeedPosition>,
    until: Option<FeedPosition>,
    limit: u64,
    stop: WalkStop,
) -> Result<(Vec<UsageRecord>, Option<FeedPosition>, usize), String> {
    let mut delivered: Vec<UsageRecord> = Vec::new();
    let mut start = start;
    let mut reached: Option<FeedPosition> = None;

    for page_number in 1..=FEED_WALK_PAGES {
        let page = plugin
            .read_feed_page(subscription, scope, start.clone(), until.clone(), limit)
            .await
            .map_err(|err| {
                format!(
                    "`read_feed_page` failed on page {page_number} of a walk over a \
                     subscription naming {meters} meter(s), at a limit of {limit} and {bound}, \
                     so what the feed delivers could not be decided: {err}",
                    meters = subscription.len(),
                    bound = match until {
                        Some(_) => "bounded by a position this backend had just issued",
                        None => "unbounded",
                    },
                )
            })?;
        delivered.extend(page.entries);

        let Some(next) = page.next else {
            if until.is_none() {
                return Err(format!(
                    "page {page_number} of a live, unbounded feed read carried no \
                     continuation. A live read carries one on every page, short and empty \
                     pages included; an absent one is reserved for a bounded replay reaching \
                     its `until`, and this read sent none, so a caller has nothing to resume \
                     from and the walk cannot go on."
                ));
            }
            return Ok((delivered, None, page_number));
        };
        if reached.as_ref() == Some(&next) {
            return Ok((delivered, Some(next), page_number));
        }
        if stop == WalkStop::FirstDelivery && !delivered.is_empty() {
            return Ok((delivered, Some(next), page_number));
        }
        reached = Some(next.clone());
        start = FeedStart::After(next);
    }

    Err(format!(
        "a feed walk at a limit of {limit} followed its cursor for {FEED_WALK_PAGES} pages \
         without the cursor ever standing still. The walk's exit is a page whose continuation \
         repeats the position it was read from, which is how a consumer learns it has reached \
         the head; a cursor that keeps moving past a ledger this suite's size is one a real \
         gateway would follow forever."
    ))
}

/// The entry ids a slice carries, in the order it carries them.
pub fn ids(entries: &[UsageRecord]) -> Vec<Uuid> {
    entries.iter().map(|entry| entry.id).collect()
}

/// Where two deliveries first disagree, rendered for a report, or `None`
/// when they agree entry for entry.
///
/// The three disagreements are told apart because they are three different
/// things to tell a plugin author: a different entry at one place, the same
/// entry with different fields, and one delivery running out before the
/// other.
pub fn first_divergence(left: &[UsageRecord], right: &[UsageRecord]) -> Option<String> {
    for (index, (left_entry, right_entry)) in left.iter().zip(right.iter()).enumerate() {
        if left_entry == right_entry {
            continue;
        }
        if left_entry.id == right_entry.id {
            return Some(format!(
                "at position {index} both carried record {id} and its fields differ between the \
                 two readings",
                id = left_entry.id,
            ));
        }
        return Some(format!(
            "at position {index} one carried record {left_id} and the other record {right_id}; \
             the first read {left_ids:?} and the second {right_ids:?}",
            left_id = left_entry.id,
            right_id = right_entry.id,
            left_ids = ids(left),
            right_ids = ids(right),
        ));
    }
    if left.len() == right.len() {
        return None;
    }
    Some(format!(
        "they agree as far as either goes and then one stops: the first carried {left_ids:?} and \
         the second {right_ids:?}",
        left_ids = ids(left),
        right_ids = ids(right),
    ))
}
