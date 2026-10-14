use uuid::Uuid;

use super::{ASSIGN_TYPE_KEY_SQL, READ_TYPE_KEY_SQL, TypeKeyCache};

/// Two arbitrary registry references. Neither is derived from an identifier:
/// the cache keys on the reference the SPI carried, and the ADR bars deriving
/// one from the other.
const TYPE_A: Uuid = Uuid::from_u128(0x5ee1_0000_0000_000a);
const TYPE_B: Uuid = Uuid::from_u128(0x5ee1_0000_0000_000b);

#[test]
fn an_unresolved_type_has_no_cached_key() {
    let cache = TypeKeyCache::default();
    assert_eq!(cache.cached(TYPE_A), None);
}

#[test]
fn a_remembered_key_is_served_and_is_per_type() {
    let cache = TypeKeyCache::default();
    cache.remember(TYPE_A, 7);
    assert_eq!(cache.cached(TYPE_A), Some(7));
    assert_eq!(
        cache.cached(TYPE_B),
        None,
        "a key belongs to its own type only"
    );
}

/// Both statements name the reference column, in the `INSERT` and in the
/// `ON CONFLICT` target alike.
///
/// The cache and the table have to agree on which column holds the meter, and
/// a statement left on the retired identifier column would fail only at
/// runtime, only against a live schema, and only on a first write of a type —
/// which is why `type_key_integration_pg` alone is not enough cover. This is
/// the fast-lane half.
///
/// The `ON CONFLICT` target is named separately from the `INSERT` column on
/// purpose: they are two independent spellings in one string, and a cutover
/// that moved the first and missed the second would still insert, then
/// arbitrate against a constraint that no longer exists.
#[test]
fn both_statements_key_on_the_registry_reference() {
    assert_eq!(
        ASSIGN_TYPE_KEY_SQL,
        "INSERT INTO usage_type_key (gts_type_uuid) VALUES ($1) \
         ON CONFLICT (gts_type_uuid) DO NOTHING"
    );
    assert_eq!(
        READ_TYPE_KEY_SQL,
        "SELECT type_key FROM usage_type_key WHERE gts_type_uuid = $1"
    );
}
