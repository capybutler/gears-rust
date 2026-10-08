//! The gear's stable fingerprint primitive.
//!
//! One function, shared by the two paths that mint a cursor fingerprint:
//! [`crate::domain::query::read_fingerprint`] on the raw path and
//! [`crate::domain::feed::subscription_fingerprint`] on the feed. Both bind a
//! request's shape into a cursor so the follow-up request can recompute the
//! value and refuse a token minted for different parameters.
//!
//! It lives here, in the gear, rather than being borrowed from
//! `toolkit_odata` — whose `short_filter_hash` is built on the same algorithm
//! — because a shared library's private helper is not an interface, and
//! widening one into an interface makes every other gear a stakeholder in a
//! value only this gear puts on the wire. `types-registry` and the OIDC authn
//! plugin reached the same conclusion for their own digests. FNV-1a is a
//! frozen public specification, so the copies cannot drift in behaviour; what
//! a local copy adds is `fingerprint_tests`, which pins the digest against
//! the published vectors. Nothing in `toolkit_odata` does that today.

/// FNV-1a 64-bit — a deterministic, non-cryptographic fingerprint.
///
/// The algorithm is a public specification (Fowler–Noll–Vo) with fixed
/// constants, so the digest is identical across Rust versions, platforms and
/// builds. That cross-process stability is the whole requirement: a
/// fingerprint is minted while serving one request and compared while serving
/// the next, which may land on another replica running another binary. A
/// [`std::hash::Hasher`] from the standard library guarantees none of it —
/// `DefaultHasher`'s algorithm is explicitly unspecified and free to change
/// between toolchains, which would silently desync digests across a rolling
/// upgrade.
///
/// Takes bytes rather than an [`std::hash::Hash`] implementor on purpose. The
/// digest commits to exactly the bytes handed in, so the caller owns the
/// encoding of its pre-image; routing the input through `Hash` would make the
/// value depend on std's byte stream for the input type, which carries its
/// own "should not be considered stable between compiler versions" warning
/// and, for `str`, appends a terminator that is no part of the string.
///
/// Not collision-resistant and not for anything security-bearing. A
/// fingerprint mismatch here rejects a cursor; it authorizes nothing.
#[must_use]
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut hash = BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "fingerprint_tests.rs"]
mod fingerprint_tests;
