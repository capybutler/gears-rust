//! The retention drive the suite needs, beside the SPI rather than on it.
//!
//! DESIGN §3.3's `feed-retention-refusal` row asserts that *"A cursor after
//! which retention has removed an entry of a subscribed GTS type is refused
//! rather than served as a short page, whatever that cursor's own age and
//! whether or not the caller's scope admitted that entry"*. A backend only
//! reaches that state once retention has actually removed something, and
//! nothing on the SPI removes anything:
//! [`UsageCollectorPluginV1`](crate::plugin_api::UsageCollectorPluginV1)
//! declares seven methods and none of them purges.
//!
//! So the drive arrives beside the SPI. [`ContractRetention`] is an optional
//! capability a backend under test exposes, rather than an eighth SPI method
//! or an out-of-band step no type describes and no run performs.
//! [`run_all_with_retention`](super::run_all_with_retention) is where it
//! reaches the checks; [`run_all`](super::run_all) takes a plugin handle and
//! a dedup level and nothing else, and the checks in
//! [`RETENTION_DRIVEN_CHECKS`](super::RETENTION_DRIVEN_CHECKS) run there
//! without the assertions that need a sweep.
//!
//! **The first check to need one was `feed-bootstrap-position`**, and it
//! needs the exemption rather than the refusal: DESIGN section 3.3 has
//! `FeedStart::Oldest` *"never refused on the retention floor"* and
//! beginning *"at the oldest entry the subscription retains"*, and neither
//! clause says anything until something has been swept.
//!
//! **The refusal itself is `feed-retention-refusal`'s, and it has landed.**
//! That is the check this trait exists for: the exemption can at least be
//! stated over an unswept ledger, where `FeedStart::Oldest` is served
//! because nothing has been removed, but
//! [`UsageCollectorPluginError::CursorBeyondRetention`](crate::error::UsageCollectorPluginError::CursorBeyondRetention)
//! has no reachable state at all until a sweep has removed something. Three
//! of that check's four assertions run only through
//! [`run_all_with_retention`](super::run_all_with_retention), which is the
//! largest share of any check in
//! [`RETENTION_DRIVEN_CHECKS`](super::RETENTION_DRIVEN_CHECKS).
//!
//! # This is a conforming capability, not a test hook
//!
//! Every real backend already runs retention. DESIGN §3.1's "Plugin-owned
//! lifecycle" row says *"Retention, backup, archival, purging, and query
//! acceleration are plugin-owned"*, and §3.10 item 6 obliges a plugin to
//! publish *"the retention it enforces per GTS type, and how it decides that
//! retention has truncated a cursor's continuation, which is the condition it
//! refuses a cursor on"*. What this trait adds is not the sweep; it is a way
//! to ask for one at a moment a check chooses, instead of waiting for the
//! backend's own timer to reach a fixture's covered period.
//!
//! That distinction is worth stating because
//! [`reference::InMemoryReferencePlugin`](super::reference::InMemoryReferencePlugin)
//! is the exemplar a plugin author reads, and its implementation of this
//! trait is production-shaped: a mark per GTS type recording what a sweep
//! removed, which is the same shape the `TimescaleDB` plugin's own DESIGN
//! gives `usage_feed_retention_marks`. The reference backend carries no
//! defect switches for the same reason — `contract_mutants` holds its
//! deliberately non-conforming subjects in a separate, test-only mirror so
//! that nothing in the exemplar exists to be copied by mistake.
//!
//! # What a porter implements
//!
//! A plugin under test implements this against whatever its storage engine
//! already does on a timer: a chunk drop, a partition detach, a `DELETE`.
//! It is not asked for a new capability, only for that one on demand. A
//! backend that cannot be driven is handed to [`run_all`](super::run_all)
//! instead, and its conformance is reported as covering less - which is
//! what [`RETENTION_DRIVEN_CHECKS`](super::RETENTION_DRIVEN_CHECKS) is for.
//!
//! **A driven check may remove entries, and has to put them back.** The
//! suite's own premise is that it never removes anything, so a repeated run
//! re-delivers identical entries and a conforming backend absorbs them; a
//! swept entry is not absorbed on re-delivery but inserted afresh, at the
//! head of the feed's order rather than where it was. Every check that
//! drives a sweep therefore restores its own meter before it returns, and
//! `the_reference_backend_conforms_to_a_repeated_run_under_a_retention_drive`
//! is what holds them to it.
//!
//! **What "back" is differs per check**, because what each of them asserts
//! about its own ledger differs. `feed-bootstrap-position` re-delivers its
//! two entries in its own order, since it asserts which of them a read
//! begins at. `feed-retention-refusal` leaves its swept meter empty, since
//! it asserts what lies *after* cursors it issues and builds the ledger in
//! front of them: an entry already on the meter would be behind the first
//! cursor rather than in front of it. Both drive over a meter derived for
//! one check's exclusive use, and [`ContractRetention::drop_before`] is
//! keyed on a GTS type, so neither restoration can reach the other's
//! fixtures however the floors compare.

use crate::models::MeterTypeId;

/// A backend under test whose retention the suite can drive.
///
/// Implemented beside
/// [`UsageCollectorPluginV1`](crate::plugin_api::UsageCollectorPluginV1)
/// rather than on it: DESIGN declares no retention method on the SPI, the
/// SPI's seven methods are the whole of it, and a check that needs a purge
/// needs it from the backend rather than from the gear. See the module docs
/// for why this is a capability every conforming backend has rather than a
/// hook written for the suite.
#[async_trait::async_trait]
pub trait ContractRetention: Send + Sync {
    /// Removes every entry of `gts_type_id` whose covered period ends before
    /// `floor`, as this backend's own retention sweep would.
    ///
    /// # The key is the covered-period end, not the acceptance instant
    ///
    /// DESIGN §3.1's "Idempotency horizon" row measures retention from the
    /// covered period: *"A dedup identity stays visible for at least the
    /// declared retention of its type, measured from the entry's
    /// `window_end`"*. A drive keyed on `accepted_at` would therefore ask a
    /// backend to remove a different set of entries than its own retention
    /// removes, and a check built on it would assert against a sweep no
    /// deployment runs.
    ///
    /// # The key is one GTS type
    ///
    /// `usage-collector-v1.yaml` states the refusal per type: *"Removal is
    /// read per subscribed GTS type, so a cursor can be refused for an entry
    /// the caller's own scope excluded"*. Retention is declared per type and
    /// read per type, so a drive that named no type would be driving
    /// something the contract does not describe.
    ///
    /// It is also what lets this suite purge at all. [`run_all`](super::run_all)
    /// dispatches every check against one backend that removes nothing of its
    /// own accord, so the checks share a ledger; a drop keyed on an instant
    /// alone would take every other check's fixtures with it. A check that
    /// drives retention writes its own meter and drops on that meter, and the
    /// rest of the ledger is untouched.
    ///
    /// # Errors
    ///
    /// A `String`, deliberately, rather than
    /// [`UsageCollectorPluginError`](crate::error::UsageCollectorPluginError):
    /// a failure here is the harness failing to set a scenario up, not a
    /// plugin answering an SPI call. Reporting it as a plugin error would put
    /// a backend's name on a fault it was never asked to have an opinion
    /// about. The detail is free text for a human reading a failed run.
    async fn drop_before(
        &self,
        gts_type_id: &MeterTypeId,
        floor: time::OffsetDateTime,
    ) -> Result<(), String>;
}
