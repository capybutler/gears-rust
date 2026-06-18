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
