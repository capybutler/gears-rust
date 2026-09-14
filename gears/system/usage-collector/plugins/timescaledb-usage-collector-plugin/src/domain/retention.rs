//! The retention sweep's decision for one ledger chunk.
//!
//! Pure: no database, no registry, no clock. The sweep supplies the chunk's
//! time range end, the retention of every type in its key range, and `now`.

use std::time::Duration;

use time::OffsetDateTime;

use crate::domain::ports::RetentionError;

/// Why a chunk is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// Every type resolved, and at least one is still inside its retention.
    NotExpired,
    /// No type maps to the chunk's key range.
    NoType,
    /// A type's retention could not be read from the registry.
    Unavailable,
    /// A type in the chunk is not registered.
    NotFound,
    /// A type in the chunk declares no retention.
    MissingTrait,
    /// A type in the chunk declares an unusable retention.
    InvalidTrait,
}

impl KeepReason {
    /// Whether the chunk is kept because a retention could not be resolved,
    /// rather than because it has not expired.
    #[must_use]
    pub const fn is_unresolved(self) -> bool {
        !matches!(self, Self::NotExpired)
    }

    /// The bounded label value for this reason.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::NotExpired => "not_expired",
            Self::NoType => "no_type",
            Self::Unavailable => "unavailable",
            Self::NotFound => "not_found",
            Self::MissingTrait => "missing_trait",
            Self::InvalidTrait => "invalid_trait",
        }
    }
}

impl From<&RetentionError> for KeepReason {
    fn from(err: &RetentionError) -> Self {
        match err {
            RetentionError::Unavailable(_) => Self::Unavailable,
            RetentionError::NotFound => Self::NotFound,
            RetentionError::MissingTrait => Self::MissingTrait,
            RetentionError::InvalidTrait(_) => Self::InvalidTrait,
        }
    }
}

/// What the sweep does with one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Drop,
    Keep(KeepReason),
}

/// Decide one chunk.
///
/// `time_end` is the exclusive end of the chunk's `window_end` range, so it
/// bounds every entry in the chunk from above: a deadline computed from it is
/// never earlier than any entry's own, and a drop never frees a dedup identity
/// before its horizon. `retentions` holds one resolution per type in the
/// chunk's key range.
///
/// The chunk is dropped only when every type resolved and the longest retention
/// has passed: `time_end + longest < now`. Any failed resolution keeps it, under
/// the first failure's reason.
#[must_use]
pub fn drop_decision(
    time_end: OffsetDateTime,
    retentions: &[Result<Duration, RetentionError>],
    now: OffsetDateTime,
) -> Decision {
    if retentions.is_empty() {
        return Decision::Keep(KeepReason::NoType);
    }
    let mut longest = Duration::ZERO;
    for retention in retentions {
        match retention {
            Ok(duration) => longest = longest.max(*duration),
            Err(err) => return Decision::Keep(KeepReason::from(err)),
        }
    }
    // A retention too long to add to an instant cannot have expired.
    let Some(deadline) = time::Duration::try_from(longest)
        .ok()
        .and_then(|duration| time_end.checked_add(duration))
    else {
        return Decision::Keep(KeepReason::NotExpired);
    };
    if deadline < now {
        Decision::Drop
    } else {
        Decision::Keep(KeepReason::NotExpired)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "retention_tests.rs"]
mod retention_tests;
