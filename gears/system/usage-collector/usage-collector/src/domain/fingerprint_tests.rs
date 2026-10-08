//! Known-answer tests for the gear's stable fingerprint primitive.
//!
//! These are not ordinary unit tests. [`fnv1a_64`]'s digests reach the wire
//! inside cursor tokens, are minted by one process and compared by another —
//! a later request, very likely another replica running another build. Every
//! other test of a fingerprint in this gear is *relational*: it asserts that
//! two calls agree or differ, which holds just as well under a silently
//! changed algorithm. Only an absolute vector catches that change, so these
//! are the tests that actually hold the cross-build guarantee
//! [`crate::domain::query::read_fingerprint`] and
//! [`crate::domain::feed::subscription_fingerprint`] document.
//!
//! The expected values are the published Fowler–Noll–Vo reference vectors for
//! the 64-bit FNV-1a variant, not values read back out of this
//! implementation. A vector regenerated from the code under test would pin
//! whatever that code happens to do; these pin the specification.
//!
//! Failing here means the digest changed. That is a wire-format break, never
//! a stale expectation to refresh — the fix is to restore the algorithm, not
//! to re-record the vector.

use super::fnv1a_64;

/// The FNV-1a offset basis, which is what the empty input must hash to: the
/// loop never runs, so the digest is the seed unmixed. Pins the basis
/// constant on its own, with the prime out of the picture.
#[test]
fn empty_input_hashes_to_the_offset_basis() {
    assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
}

/// The published single-byte vectors. Together with the empty case these pin
/// the prime: one multiply separates each from the basis, so a wrong prime
/// cannot survive them.
#[test]
fn single_byte_inputs_match_the_reference_vectors() {
    assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a_64(b"b"), 0xaf63_df4c_8601_f1a5);
    assert_eq!(fnv1a_64(b"c"), 0xaf63_de4c_8601_eff2);
}

/// The published multi-byte vector. Catches a fold that is right for one
/// round and wrong when iterated — a misplaced wrapping multiply, an xor and
/// multiply swapped into FNV-1 order.
#[test]
fn multi_byte_input_matches_the_reference_vector() {
    assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
}

/// Non-ASCII bytes hash as bytes. The fingerprint pre-images carry
/// caller-supplied metadata values and GTS type ids, so a multi-byte UTF-8
/// sequence is ordinary input here, and this fixes that it is absorbed as
/// its UTF-8 encoding rather than, say, per-`char` code points.
#[test]
fn multi_byte_utf8_is_absorbed_as_its_encoding() {
    assert_eq!(fnv1a_64("\u{e9}".as_bytes()), 0x0ac2_1707_b718_1e01);
    assert_eq!(fnv1a_64("\u{e9}".as_bytes()), fnv1a_64(&[0xc3, 0xa9]));
}

/// Order is part of the digest. The pre-images this hashes are
/// field-concatenations, so a commutative fold would let two different
/// queries share a fingerprint and silently accept each other's cursors.
#[test]
fn transposed_bytes_hash_differently() {
    assert_ne!(fnv1a_64(b"ab"), fnv1a_64(b"ba"));
}
