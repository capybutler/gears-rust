use super::*;
use sqlx::postgres::PgSslMode;

// `PgSslMode` derives no `PartialEq`, so assertions match the variant.

#[test]
fn connect_options_upgrades_prefer_to_require() {
    let opts = connect_options("postgres://u:p@h/db?sslmode=prefer").expect("valid dsn");
    assert!(
        matches!(opts.get_ssl_mode(), PgSslMode::Require),
        "a weaker `prefer` mode must be upgraded to `require`"
    );
}

#[test]
fn connect_options_upgrades_allow_to_require() {
    let opts = connect_options("postgres://u:p@h/db?sslmode=allow").expect("valid dsn");
    assert!(
        matches!(opts.get_ssl_mode(), PgSslMode::Require),
        "a silent `allow` fallback must be upgraded to `require`"
    );
}

#[test]
fn connect_options_honors_explicit_disable() {
    // An explicit `disable` is a deliberate, auditable opt-out (local / tests).
    let opts = connect_options("postgres://u:p@h/db?sslmode=disable").expect("valid dsn");
    assert!(
        matches!(opts.get_ssl_mode(), PgSslMode::Disable),
        "an explicit `disable` is a deliberate opt-out and must be honored"
    );
}

#[test]
fn connect_options_defaults_unspecified_dsn_to_require() {
    // sqlx's default is `prefer` (plaintext fallback); enforcement makes it `require`.
    let opts = connect_options("postgres://u:p@h/db").expect("valid dsn");
    assert!(
        matches!(opts.get_ssl_mode(), PgSslMode::Require),
        "a DSN without an explicit sslmode must default to `require`, not `prefer`"
    );
}

#[test]
fn connect_options_preserves_stronger_verify_full() {
    let opts = connect_options("postgres://u:p@h/db?sslmode=verify-full").expect("valid dsn");
    assert!(
        matches!(opts.get_ssl_mode(), PgSslMode::VerifyFull),
        "an operator's stronger `verify-full` must not be downgraded to `require`"
    );
}

#[test]
fn connect_options_rejects_malformed_dsn() {
    assert!(connect_options("not a dsn").is_err());
}

#[test]
fn connection_gucs_bind_both_timeouts_and_the_fixed_lock_timeout() {
    // Both the statement and the transaction timeout are config-driven
    // (seconds -> `<n>s`) and distinct values are passed so one cannot stand in
    // for the other; the lock timeout is a fixed constant so a contended lock
    // fails fast rather than blocking.
    // Compared whole, as slices, rather than index by index: a GUC added to the
    // set would otherwise slip past assertions that only pin the entries already
    // there, and comparing as slices makes the added entry a test failure naming
    // it rather than a type error on the array length.
    assert_eq!(
        connection_gucs(45, 90).as_slice(),
        [
            ("statement_timeout", "45s".to_owned()),
            ("transaction_timeout", "90s".to_owned()),
            ("lock_timeout", LOCK_TIMEOUT.to_owned()),
        ]
        .as_slice()
    );
}

#[test]
fn pool_connect_options_sets_both_timeouts_and_the_lock_timeout() {
    // The request-path connect options must carry the GUCs as `-c` startup
    // parameters so every pooled connection is bounded at connect time.
    let opts =
        pool_connect_options("postgres://u:p@h/db?sslmode=require", 45, 90).expect("valid dsn");
    let applied = opts.get_options().expect("runtime options must be set");
    assert!(
        applied.contains("statement_timeout=45s"),
        "statement_timeout GUC missing; got: {applied}"
    );
    assert!(
        applied.contains("transaction_timeout=90s"),
        "transaction_timeout GUC missing; got: {applied}"
    );
    assert!(
        applied.contains("lock_timeout=5s"),
        "lock_timeout GUC missing; got: {applied}"
    );
    // TLS enforcement from `connect_options` still applies through the same builder.
    assert!(
        matches!(opts.get_ssl_mode(), PgSslMode::Require),
        "pool_connect_options must preserve TLS enforcement"
    );
}

#[test]
fn is_plaintext_only_true_for_disable() {
    // Only an explicit `disable` reaches here as plaintext: the silent fallbacks
    // (`prefer`/`allow`) are upgraded to `require` before this predicate runs.
    assert!(is_plaintext(PgSslMode::Disable));
    assert!(!is_plaintext(PgSslMode::Require));
    assert!(!is_plaintext(PgSslMode::VerifyCa));
    assert!(!is_plaintext(PgSslMode::VerifyFull));
}
