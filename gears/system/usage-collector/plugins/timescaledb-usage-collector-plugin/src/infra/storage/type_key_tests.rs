use super::TypeKeyCache;

#[test]
fn an_unresolved_type_has_no_cached_key() {
    let cache = TypeKeyCache::default();
    assert_eq!(
        cache.cached("gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~"),
        None
    );
}

#[test]
fn a_remembered_key_is_served_and_is_per_type() {
    let cache = TypeKeyCache::default();
    cache.remember("type-a~", 7);
    assert_eq!(cache.cached("type-a~"), Some(7));
    assert_eq!(
        cache.cached("type-b~"),
        None,
        "a key belongs to its own type only"
    );
}
